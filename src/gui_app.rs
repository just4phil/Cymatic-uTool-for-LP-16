//! The lp16-tool GUI (egui/eframe) - a rebuild of the uTool2 main window.
//!
//! Layout, colours and wording follow the screenshots of the original in
//! uTool2 V2.0.062 (kept locally in _private/docs/utool2-screens):
//! - seven tabs (Home, Rehearsal, Export, Import, Edit Target, Settings, Support);
//!   Rehearsal is new (the player and its mixer), see gui_rehearsal.rs
//! - two-column pages: explorer tree on the left, details on the right
//! - editors and dialogs are overlays inside the main window: black
//!   backdrop, orange title, dark rounded box, orange-framed buttons.
//!
//! Everything goes through the same lp16_tool library that the CLI uses.

use crate::config_path;
use crate::gui_audio::AudioPanel;
use crate::gui_backup::BackupPanel;
use crate::gui_bandpack::BandPack;
use crate::gui_midi::MidiPanel;
use crate::gui_rehearsal::RehearsalPanel;
use crate::gui_setlist_print::SetlistPrint;
use crate::gui_tail::{TailCheck, TailCheckKind};
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, FontId, Layout, Margin, Rect, RichText, Sense,
    Stroke, Ui,
};
use lp16_tool::audio_out::AudioSettings;
use lp16_tool::mix::MixSaver;
use lp16_tool::project_lib::ProjectLibrary;
use lp16_tool::setlist::{self, SetMarkers, Timeline};
use lp16_tool::song_info::{self, SongInfo as SongFacts};
use lp16_tool::sync::{self, Action, ExportMode, Progress};
use lp16_tool::wav_tail::{self, TailReport};
use lp16_tool::i18n::{self, Lang};
use lp16_tool::tr;
use lp16_tool::{
    import_songs, init_folders,
    library_songs, midi_duration, natural_cmp, newest_version, scan_import, ImportItem, ImportStatus, LibrarySong,
    now_ms, resolve, save_backup, save_config, save_mixes, save_set_pause, save_setlist_logo, MAX_SET_PAUSE,
    unresolve, valid_fat32_name, Config,
    delay_seconds, start_mode_text, PlaylistSong, ProjectTrack, SongProject, StartMode, WavInfo,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// ---------------------------------------------------------------------------
// colours (sampled from the uTool2 screenshots)
// ---------------------------------------------------------------------------

pub(crate) const ORANGE: Color32 = Color32::from_rgb(0xf0, 0x6b, 0x19);
const BG: Color32 = Color32::from_rgb(0x30, 0x30, 0x30);
pub(crate) const PANEL: Color32 = Color32::from_rgb(0x40, 0x40, 0x40);
const TAB_IDLE: Color32 = Color32::from_rgb(0x48, 0x48, 0x48);
const TAB_HOVER: Color32 = Color32::from_rgb(0x58, 0x58, 0x58);
const FIELD: Color32 = Color32::from_rgb(0x60, 0x60, 0x60);
const LIST: Color32 = Color32::from_rgb(0x30, 0x30, 0x30);
const SEL: Color32 = Color32::from_rgb(0x5a, 0x5a, 0x5a);
const HOVER: Color32 = Color32::from_rgb(0x4a, 0x4a, 0x4a);
pub(crate) const LINE: Color32 = Color32::from_rgb(0x4c, 0x4c, 0x4c);
pub(crate) const TEXT: Color32 = Color32::from_rgb(0xf0, 0xf0, 0xf0);
pub(crate) const DIM: Color32 = Color32::from_rgb(0x9a, 0x9a, 0x9a);
const GREEN: Color32 = Color32::from_rgb(0x6c, 0xd8, 0x6c);
pub(crate) const RED: Color32 = Color32::from_rgb(0xf0, 0x5a, 0x5a);
const MARK_FILL: Color32 = Color32::from_rgb(0xfb, 0xd9, 0xc4);

const ROW_H: f32 = 22.0;
/// playlist editor: marker rows (encores, end of concert)
pub const MARKER_FILL: Color32 = Color32::from_rgb(0x5a, 0x40, 0x32);
const MARKER_SEL: Color32 = Color32::from_rgb(0x7a, 0x52, 0x3a);
/// playlist editor: centre of the Start and End columns, from the right edge
const COL_START: f32 = 320.0;
const COL_END: f32 = 262.0;

// ---------------------------------------------------------------------------
// data
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Home,
    Rehearsal,
    Export,
    Backup,
    Import,
    EditTarget,
    Settings,
    Support,
}

fn tabs() -> [(Tab, &'static str); 8] { [
    (Tab::Home, tr!("tab.home")),
    (Tab::Rehearsal, tr!("tab.rehearsal")),
    (Tab::Export, tr!("tab.export")),
    (Tab::Backup, tr!("tab.backup")),
    (Tab::Import, tr!("tab.import")),
    (Tab::EditTarget, tr!("tab.edit_target")),
    (Tab::Settings, tr!("tab.settings")),
    (Tab::Support, tr!("tab.support")),
] }

struct TrackInfo {
    num: i32,
    name: String,
    wav: String,
    exists: bool,
    /// end of the WAV (`wav_tail::analyze_tail`): None = not looked at yet,
    /// Some(None) = clean. Filled when the song is shown in Home.
    tail: Option<Option<TailReport>>,
    /// size and modification time of the WAV: a refresh keeps `tail` while
    /// they are the same
    stamp: (u64, i64),
}

struct SongInfo {
    path: String,
    name: String,
    sr: u32,
    bits: u32,
    duration: f64,
    tracks: Vec<TrackInfo>,
    midi_name: String,
    midi_full: String,
    midi_exists: bool,
    /// the song has its own mixer values (`mix::has_song_mix`)
    song_mix: bool,
    /// name on the printed setlist (`.songinfo`); empty = not set
    print_title: String,
    size: u64,
    modified: i64,
}

impl SongInfo {
    fn warn(&self) -> bool {
        self.tracks.iter().any(|t| !t.exists || matches!(t.tail, Some(Some(_))))
            || (!self.midi_full.is_empty() && !self.midi_exists)
    }
    fn format_line(&self) -> String {
        tr!("home.track_fmt", n = self.tracks.len(), bits = self.bits)
    }
}

/// One song entry of a playlist.
#[derive(Clone, PartialEq)]
struct PlSong {
    /// full path of the .songprj
    prj: String,
    mode: String,
    delay: String,
}

struct PlaylistInfo {
    name: String,
    songs: Vec<PlSong>,
    /// encores / end of concert
    markers: SetMarkers,
    size: u64,
    modified: i64,
}

#[derive(Clone, Copy, PartialEq)]
enum Sel {
    Root,
    SongsFolder,
    PlaylistsFolder,
    Song(usize),
    Playlist(usize),
}

#[derive(Clone, Copy, PartialEq)]
enum Check {
    Off,
    On,
    Partial,
}

/// Pending dialog (uTool2 wording, from screenshots of the original).
enum Dialog {
    Message { title: String, text: String },
    SongName { index: usize, name: String, copy_only: bool },
    PlaylistName { index: usize, name: String, copy_only: bool },
    /// "New Multitrack Song": name and audio format
    NewSong { name: String, sr: u32, bits: u32 },
    DeleteSong(usize),
    DeletePlaylist(usize),
    /// playlist editor: the name was changed, rename or copy?
    EditorSaveAs,
    /// playlist editor: close with unsaved changes?
    EditorClose,
    /// song editor: close with unsaved changes?
    SongEditorClose,
    /// import tab: import the checked songs?
    ImportConfirm,
    /// Home: repair WAVs without a clean end of song `song`
    /// (track name, wav, finding); `trim_ms` is cut before the fade-out
    FixTail { song: usize, tracks: Vec<(String, String, TailReport)>, trim_ms: f64 },
    /// export: songs to export end without a clean fade-out, export anyway?
    ExportTails(String),
    /// Edit Target: delete a song folder from the stick (`playlists` still list it)
    EtDeleteSong { name: String, playlists: Vec<String> },
    /// Edit Target: delete a .play from the stick
    EtDeletePlaylist(String),
    /// Edit Target: delete every song no playlist lists (names, total bytes)
    EtDeleteUnused(Vec<String>, u64),
}

/// "Update Playlist" overlay: a new playlist from an existing one, every song
/// replaced by its newest version (same song folder, later date folder).
struct PlUpdate {
    source: usize,
    name: String,
    /// headline, e.g. "3 Song(s) imported."
    info: String,
    lib: Vec<LibrarySong>,
    /// per song of the source playlist: replacement (index into lib), None = keep
    picks: Vec<Option<usize>>,
    /// .songprj paths (lower case) imported just now
    fresh: HashSet<String>,
}

/// "Edit Multitrack Playlist" overlay state (a working copy).
struct PlaylistEditor {
    /// None = new playlist
    index: Option<usize>,
    name: String,
    /// songs and the two concert markers, in list order
    items: Vec<PlItem>,
    checked: Vec<bool>,
    /// pause between two songs (the global setting, edited here)
    pause: u32,
    /// start mode and delay (s) that "Apply to selected rows" sets
    start_mode: StartMode,
    start_delay: u32,
    dirty: bool,
    avail_open: bool,
    avail_sel: Option<usize>,
    /// start of a Shift range in the Playlist Content
    anchor: Option<usize>,
    /// what is being dragged onto the Playlist Content
    drag: Option<PlDrag>,
}

/// A row of the Playlist Content.
#[derive(Clone, PartialEq)]
enum PlItem {
    Song(PlSong),
    /// the encores start here
    Encore,
    /// the concert ends here; the songs below are for information only
    End,
}

/// The editor rows of a playlist: its songs with the markers in between.
fn items_from(songs: &[PlSong], m: &SetMarkers) -> Vec<PlItem> {
    let mut items = Vec::with_capacity(songs.len() + 2);
    for g in 0..=songs.len() {
        if m.encore == Some(g) {
            items.push(PlItem::Encore);
        }
        if m.end == Some(g) {
            items.push(PlItem::End);
        }
        if let Some(s) = songs.get(g) {
            items.push(PlItem::Song(s.clone()));
        }
    }
    items
}

/// The songs of the editor rows, and the markers as gap indices.
fn split_items(items: &[PlItem]) -> (Vec<PlSong>, SetMarkers) {
    let mut songs = Vec::new();
    let mut m = SetMarkers::default();
    for it in items {
        match it {
            PlItem::Song(s) => songs.push(s.clone()),
            PlItem::Encore => m.encore = Some(songs.len()),
            PlItem::End => m.end = Some(songs.len()),
        }
    }
    (songs, m)
}

fn item_label(item: &PlItem) -> String {
    match item {
        PlItem::Song(s) => stem(&s.prj),
        PlItem::Encore => tr!("common.encores").into(),
        PlItem::End => tr!("common.end_of_concert").into(),
    }
}

/// A song added in the playlist editor.
fn new_pl_song(prj: &str) -> PlSong {
    PlSong { prj: prj.to_string(), mode: StartMode::Button.as_str().into(), delay: "0".into() }
}

#[derive(Clone, Copy, PartialEq)]
enum PlDrag {
    /// the checked rows of the Playlist Content (reordering)
    Rows,
    /// a song from Available Songs (index into `songs`)
    Song(usize),
}

/// Move the checked entries of `items` as one block into the gap `gap`
/// (0 = before the first entry, len = after the last), keeping their order.
/// Returns the new check marks (the moved block stays checked).
fn move_checked<T>(items: &mut Vec<T>, checked: &[bool], gap: usize) -> Vec<bool> {
    let gap = gap.min(items.len());
    let before = checked[..gap].iter().filter(|&&c| c).count();
    let mut block = Vec::new();
    let mut rest = Vec::new();
    for (it, &c) in items.drain(..).zip(checked) {
        if c {
            block.push(it);
        } else {
            rest.push(it);
        }
    }
    let at = gap - before;
    let n = block.len();
    items.extend(rest.drain(..at));
    items.extend(block);
    items.extend(rest);
    (0..items.len()).map(|i| i >= at && i < at + n).collect()
}

/// Label next to the mouse pointer while something is dragged.
fn paint_drag_ghost(ctx: &egui::Context, text: String) {
    let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else { return };
    ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
    let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("drag_ghost")));
    let galley = p.layout_no_wrap(text, FontId::proportional(13.0), TEXT);
    let r = Rect::from_min_size(pos + vec2(14.0, 10.0), galley.size() + vec2(12.0, 6.0));
    p.rect_filled(r, 3.0, Color32::from_rgb(0x2a, 0x2a, 0x2a));
    p.rect_stroke(r, 3.0, Stroke::new(1.0_f32, ORANGE));
    p.galley(r.min + vec2(6.0, 3.0), galley, TEXT);
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    /// not selected
    None,
    /// selected song, up to date on the target: nothing is written
    Skip,
    /// selected playlist, already on the target (its .play is always written)
    Quick,
    /// selected song, changed: written again
    Overwrite,
    New,
    Force,
}

fn song_mark(action: Action) -> Mark {
    match action {
        Action::Skip => Mark::Skip,
        Action::New => Mark::New,
        Action::Quick => Mark::Overwrite,
        Action::Force => Mark::Force,
    }
}

fn playlist_mark(action: Action) -> Mark {
    match action {
        Action::Skip | Action::Quick => Mark::Quick,
        Action::New => Mark::New,
        Action::Force => Mark::Force,
    }
}

/// Export tab preview: what would happen on the target.
struct Preview {
    key: String,
    target_ok: bool,
    music: bool,
    songs: Vec<(String, Mark)>,
    plays: Vec<(String, Mark)>,
    selected: u64,
    to_copy: u64,
    free: Option<u64>,
}

/// Background export job (progress + report).
struct ExportJob {
    total: usize,
    done: usize,
    current: String,
    lines: Vec<(String, bool, String)>,
    finished: bool,
    cancel: bool,
    started: Instant,
    elapsed: f64,
}

#[derive(Clone, PartialEq)]
enum EtSel {
    Song(String),
    Play(String),
}

/// What the Target Explorer was asked to do this frame.
enum EtAction {
    Delete(EtSel),
    DeleteUnused,
}

struct EtDetails {
    title: String,
    lines: Vec<(String, String)>,
    list_title: String,
    list: Vec<String>,
    play_songs: bool,
    /// start mode and delay of each song of a .play
    starts: Vec<(String, String)>,
}

pub struct Lp16App {
    config: Config,
    tab: Tab,
    /// the tab of the last frame (to see a switch to Home)
    shown_tab: Tab,
    songs: Vec<SongInfo>,
    song_idx: HashMap<String, usize>,
    playlists: Vec<PlaylistInfo>,
    sel: Sel,
    open: HashSet<String>,
    drives: Vec<(String, String)>,
    // export
    force: bool,
    x_songs: HashSet<usize>,
    x_playlists: HashSet<usize>,
    preview: Option<Preview>,
    preview_gen: u64,
    export_job: Option<Arc<Mutex<ExportJob>>>,
    folder_edit: bool,
    // edit target
    et_sel: Option<EtSel>,
    et_cache: Option<(EtSel, EtDetails)>,
    /// the selectable rows of the Target Explorer in the last frame (for the arrow keys)
    et_rows: Vec<EtSel>,
    /// scroll the selected Target Explorer row into view
    et_scroll: bool,
    // overlays
    dialog: Option<Dialog>,
    pl_editor: Option<PlaylistEditor>,
    song_ed: Option<SongEditor>,
    settings_dirty: bool,
    // import: result of the last scan, check marks, filter
    import_scan: Option<Result<Vec<ImportItem>, String>>,
    import_check: Vec<bool>,
    import_show_all: bool,
    pl_update: Option<PlUpdate>,
    // rehearsal player
    rehearsal: RehearsalPanel,
    /// decides when changed rehearsal mix values are written to the config
    mix_saver: MixSaver,
    // the "Export Stereo-MP3" tab and when its settings are saved
    backup: BackupPanel,
    backup_saver: MixSaver,
    /// band package export / import (`.lp16pack`)
    bandpack: BandPack,
    /// "Print setlist" of a playlist (Word file)
    setlist_print: SetlistPrint,
    // settings: the "Audio output" area
    audio: AudioPanel,
    // settings: the "MIDI output" area
    midi: MidiPanel,
    // WAV endings: the running check (with the song keys it covers), checks
    // waiting for it, and its message once no dialog is open
    tail_check: Option<(TailCheck, Vec<String>)>,
    tail_queue: Vec<(TailCheckKind, Option<Vec<String>>)>,
    tail_msg: Option<String>,
}

// ---------------------------------------------------------------------------
// helpers: paths, formatting, Windows bits
// ---------------------------------------------------------------------------

/// The comparison key of a song / playlist path (see `lp16_tool::path_key`).
pub(crate) fn key(path: &str) -> String {
    lp16_tool::path_key::path_key(path)
}

/// "\Recording" or "\Recording\<sub>": where songs and playlists go on the stick
fn stick_location(sub: &str) -> String {
    if sub.is_empty() {
        format!("\\{}", sync::RECORDING)
    } else {
        format!("\\{}\\{}", sync::RECORDING, sub)
    }
}

fn stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map(|x| x.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn mtime(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn fmt_len(sec: f64) -> String {
    let s = sec.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// A point in time of the concert, always h:mm:ss (like the old sheet).
fn fmt_clock(sec: f64) -> String {
    let s = sec.max(0.0).round() as u64;
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

pub(crate) fn fmt_size(b: u64) -> String {
    let f = b as f64;
    if f < 1024.0 {
        format!("{} B", b)
    } else if f < 1024.0 * 1024.0 {
        format!("{:.1} KB", f / 1024.0)
    } else if f < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB", f / 1048576.0)
    } else {
        format!("{:.1} GB", f / 1073741824.0)
    }
}

fn fmt_sr(sr: u32) -> String {
    if sr % 1000 == 0 {
        format!("{} kHz", sr / 1000)
    } else {
        format!("{:.1} kHz", sr as f64 / 1000.0)
    }
}

pub(crate) fn fmt_date(ms: i64) -> String {
    if ms <= 0 {
        return "-".into();
    }
    let (y, mo, d, h, mi) = local_time(ms);
    if i18n::lang() == Lang::De {
        return format!("{}.{}.{} {}:{:02}", d, mo, y, h, mi);
    }
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (h12, ap) = match h {
        0 => (12, "am"),
        1..=11 => (h, "am"),
        12 => (12, "pm"),
        _ => (h - 12, "pm"),
    };
    format!(
        "{} {} {} {}:{:02}{}",
        d,
        MON[(mo.clamp(1, 12) - 1) as usize],
        y,
        h12,
        mi,
        ap
    )
}

/// days since 1970-01-01 -> (year, month, day), proleptic Gregorian
#[allow(dead_code)]
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(windows)]
#[allow(non_snake_case)]
mod win {
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct FileTime {
        pub lo: u32,
        pub hi: u32,
    }
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct SystemTime {
        pub year: u16,
        pub month: u16,
        pub dow: u16,
        pub day: u16,
        pub hour: u16,
        pub minute: u16,
        pub second: u16,
        pub ms: u16,
    }
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetLogicalDrives() -> u32;
        pub fn GetDriveTypeW(root: *const u16) -> u32;
        pub fn GetVolumeInformationW(
            root: *const u16,
            name: *mut u16,
            name_len: u32,
            serial: *mut u32,
            max_comp: *mut u32,
            flags: *mut u32,
            fs_name: *mut u16,
            fs_len: u32,
        ) -> i32;
        pub fn GetDiskFreeSpaceExW(
            dir: *const u16,
            avail: *mut u64,
            total: *mut u64,
            free: *mut u64,
        ) -> i32;
        pub fn FileTimeToLocalFileTime(ft: *const FileTime, out: *mut FileTime) -> i32;
        pub fn FileTimeToSystemTime(ft: *const FileTime, st: *mut SystemTime) -> i32;
    }
    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    pub fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }
}

#[cfg(windows)]
pub(crate) fn local_time(ms: i64) -> (i64, u32, u32, u32, u32) {
    let ticks = (ms as u64) * 10_000 + 116_444_736_000_000_000;
    let ft = win::FileTime {
        lo: ticks as u32,
        hi: (ticks >> 32) as u32,
    };
    let mut local = win::FileTime::default();
    let mut st = win::SystemTime::default();
    unsafe {
        if win::FileTimeToLocalFileTime(&ft, &mut local) == 0
            || win::FileTimeToSystemTime(&local, &mut st) == 0
        {
            return (1970, 1, 1, 0, 0);
        }
    }
    (
        st.year as i64,
        st.month as u32,
        st.day as u32,
        st.hour as u32,
        st.minute as u32,
    )
}

#[cfg(not(windows))]
pub(crate) fn local_time(ms: i64) -> (i64, u32, u32, u32, u32) {
    let secs = ms / 1000;
    let (y, m, d) = civil_from_days(secs.div_euclid(86400));
    let rem = secs.rem_euclid(86400);
    (y, m, d, (rem / 3600) as u32, (rem / 60 % 60) as u32)
}

/// (root, display) of the drives that can be a target, e.g.
/// ("D:\\", "D: LP16 (FAT32)")
#[cfg(windows)]
fn list_drives() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mask = unsafe { win::GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{}:\\", letter);
        let w = win::wide(&root);
        let kind = unsafe { win::GetDriveTypeW(w.as_ptr()) };
        if kind != 2 && kind != 3 {
            continue; // removable or fixed only
        }
        let mut name = [0u16; 128];
        let mut fsn = [0u16; 64];
        let ok = unsafe {
            win::GetVolumeInformationW(
                w.as_ptr(),
                name.as_mut_ptr(),
                name.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fsn.as_mut_ptr(),
                fsn.len() as u32,
            )
        };
        let display = if ok != 0 {
            let label = win::from_wide(&name);
            let fs_name = win::from_wide(&fsn);
            if label.is_empty() {
                format!("{}: ({})", letter, fs_name)
            } else {
                format!("{}: {} ({})", letter, label, fs_name)
            }
        } else {
            format!("{}:", letter)
        };
        out.push((root, display));
    }
    out
}

#[cfg(not(windows))]
fn list_drives() -> Vec<(String, String)> {
    Vec::new()
}

#[cfg(windows)]
pub(crate) fn disk_free(path: &str) -> Option<u64> {
    let w = win::wide(path);
    let mut avail = 0u64;
    let ok = unsafe {
        win::GetDiskFreeSpaceExW(
            w.as_ptr(),
            &mut avail,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        Some(avail)
    } else {
        None
    }
}

#[cfg(not(windows))]
pub(crate) fn disk_free(_path: &str) -> Option<u64> {
    None
}


fn to_pl_songs(songs: &[PlSong]) -> Vec<PlaylistSong> {
    songs
        .iter()
        .map(|s| PlaylistSong {
            name: stem(&s.prj),
            songprj: s.prj.clone(),
            mode: s.mode.clone(),
            delay: s.delay.clone(),
        })
        .collect()
}

fn load_song(path: &Path, audio_lib: &str) -> SongInfo {
    let p = path.to_string_lossy().into_owned();
    // an unreadable project shows up as an empty song (the export refuses it)
    let prj = SongProject::load(&p, audio_lib).unwrap_or_else(|_| {
        let mut empty = SongProject::parse("", audio_lib);
        empty.name = stem(&p);
        empty
    });
    let mut size = 0u64;
    let tracks = prj
        .tracks
        .iter()
        .map(|t| {
            let wav = prj.wav(t);
            let meta = fs::metadata(&wav);
            if let Ok(m) = &meta {
                size += m.len();
            }
            TrackInfo {
                num: t.channel + 1,
                // prefer the track name stored in the project
                name: if t.name.is_empty() { stem(&wav) } else { t.name.clone() },
                exists: meta.is_ok(),
                stamp: (meta.as_ref().map(|m| m.len()).unwrap_or(0), mtime(Path::new(&wav))),
                wav,
                tail: None,
            }
        })
        .collect();
    let midi_full = prj.midi_path().unwrap_or_default();
    SongInfo {
        name: prj.name.clone(),
        sr: prj.sampling_rate,
        bits: prj.resolution,
        duration: prj.duration(),
        tracks,
        midi_name: Path::new(&midi_full)
            .file_name()
            .map(|x| x.to_string_lossy().into_owned())
            .unwrap_or_default(),
        midi_exists: !midi_full.is_empty() && Path::new(&midi_full).exists(),
        midi_full,
        song_mix: lp16_tool::mix::has_song_mix(&prj),
        print_title: song_info::load(path).print_title,
        size,
        modified: mtime(path),
        path: p,
    }
}

// ---------------------------------------------------------------------------
// painting helpers (tree rows, badges, buttons, panels, overlays)
// ---------------------------------------------------------------------------

struct RowOut {
    clicked: bool,
    double: bool,
    toggle: bool,
    check: bool,
    drag_started: bool,
    /// for the right-click context menu
    resp: egui::Response,
}

/// One explorer row like uTool2: arrow, optional checkbox, icon, label;
/// the right-hand columns are painted by `right`.
#[allow(clippy::too_many_arguments)]
fn tree_row(
    ui: &mut Ui,
    depth: usize,
    arrow: Option<bool>,
    check: Option<(Check, bool)>,
    icon: &str,
    label: &str,
    selected: bool,
    line: bool,
    right: impl FnOnce(&egui::Painter, Rect),
) -> RowOut {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, ROW_H), Sense::click_and_drag());
    let p = ui.painter_at(rect);
    if selected {
        p.rect_filled(rect, 0.0, SEL);
    } else if resp.hovered() {
        p.rect_filled(rect, 0.0, HOVER);
    }
    if line {
        p.hline(
            rect.x_range(),
            rect.bottom() - 0.5,
            Stroke::new(1.0_f32, LINE),
        );
    }
    let cy = rect.center().y;
    let mut x = rect.left() + 8.0 + depth as f32 * 20.0;
    let arrow_rect = Rect::from_min_size(pos2(x - 4.0, rect.top()), vec2(20.0, ROW_H));
    if let Some(open) = arrow {
        let c = pos2(x + 5.0, cy);
        let pts = if open {
            vec![pos2(c.x - 5.0, c.y - 3.0), pos2(c.x + 5.0, c.y - 3.0), pos2(c.x, c.y + 4.0)]
        } else {
            vec![pos2(c.x - 3.0, c.y - 5.0), pos2(c.x + 4.0, c.y), pos2(c.x - 3.0, c.y + 5.0)]
        };
        p.add(egui::Shape::convex_polygon(pts, DIM, Stroke::NONE));
    }
    x += 18.0;
    let mut check_rect = Rect::NOTHING;
    if let Some((state, enabled)) = check {
        check_rect = Rect::from_center_size(pos2(x + 7.0, cy), vec2(14.0, 14.0));
        paint_check(&p, check_rect, state, enabled);
        check_rect = check_rect.expand(3.0);
        x += 22.0;
    }
    if !icon.is_empty() {
        p.text(pos2(x, cy), Align2::LEFT_CENTER, icon, FontId::proportional(15.0), TEXT);
        x += 22.0;
    }
    p.text(pos2(x, cy), Align2::LEFT_CENTER, label, FontId::proportional(14.0), TEXT);
    right(&p, rect);
    let pos = resp.interact_pointer_pos();
    let in_arrow = pos.map(|q| arrow_rect.contains(q)).unwrap_or(false);
    let in_check = pos.map(|q| check_rect.contains(q)).unwrap_or(false);
    let check_enabled = matches!(check, Some((_, true)));
    RowOut {
        clicked: resp.clicked() && !in_arrow && !in_check,
        double: resp.double_clicked(),
        toggle: arrow.is_some() && ((resp.clicked() && in_arrow) || resp.double_clicked()),
        check: check_enabled && resp.clicked() && in_check,
        drag_started: resp.drag_started() && !in_arrow && !in_check,
        resp,
    }
}

/// Explorer-style click on row `i` of a checkable list: a plain click selects
/// only this row, Shift selects the range from the anchor, Ctrl or a click on
/// the checkbox toggles the row. `usable` filters rows that cannot be selected.
fn list_click(checked: &mut [bool], anchor: &mut Option<usize>, i: usize, m: egui::Modifiers, in_cb: bool, usable: impl Fn(usize) -> bool) {
    if in_cb || m.command {
        checked[i] = !checked[i];
    } else if let (true, Some(a)) = (m.shift, *anchor) {
        let (lo, hi) = (a.min(i), a.max(i));
        for (j, c) in checked.iter_mut().enumerate() {
            *c = j >= lo && j <= hi && usable(j);
        }
    } else {
        checked.iter_mut().for_each(|c| *c = false);
        checked[i] = true;
    }
    if !m.shift || anchor.is_none() {
        *anchor = Some(i);
    }
}

/// The "Entf"/Delete key was pressed while no text field has the focus.
fn delete_key(ui: &Ui) -> bool {
    ui.input(|i| i.key_pressed(egui::Key::Delete)) && ui.memory(|m| m.focused().is_none())
}

fn paint_check(p: &egui::Painter, r: Rect, state: Check, enabled: bool) {
    let fill = if !enabled {
        Color32::from_rgb(0x55, 0x55, 0x55)
    } else if state == Check::Partial {
        ORANGE
    } else {
        Color32::WHITE
    };
    p.rect_filled(r, 2.0, fill);
    if state == Check::On {
        let s = Stroke::new(2.2_f32, ORANGE);
        p.line_segment([pos2(r.left() + 3.0, r.center().y), pos2(r.left() + 6.0, r.bottom() - 3.5)], s);
        p.line_segment([pos2(r.left() + 6.0, r.bottom() - 3.5), pos2(r.right() - 2.5, r.top() + 2.5)], s);
    }
}

fn paint_warn(p: &egui::Painter, rect: Rect) {
    let r = Rect::from_center_size(pos2(rect.right() - 14.0, rect.center().y), vec2(11.0, 16.0));
    p.rect_filled(r, 3.0, ORANGE);
    p.text(r.center(), Align2::CENTER_CENTER, "!", FontId::proportional(11.0), Color32::WHITE);
}

/// format column ("48 kHz" over "13 ch - 24-bit") and length, like uTool2
fn paint_song_cols(p: &egui::Painter, rect: Rect, s: &SongInfo) {
    let x = rect.right() - 120.0;
    p.text(
        pos2(x, rect.top() + 6.0),
        Align2::CENTER_CENTER,
        fmt_sr(s.sr),
        FontId::proportional(10.0),
        TEXT,
    );
    p.text(
        pos2(x, rect.bottom() - 6.0),
        Align2::CENTER_CENTER,
        s.format_line(),
        FontId::proportional(10.0),
        TEXT,
    );
    p.text(
        pos2(rect.right() - 30.0, rect.center().y),
        Align2::RIGHT_CENTER,
        fmt_len(s.duration),
        FontId::proportional(11.5),
        TEXT,
    );
    if s.song_mix {
        crate::gui_mixer::paint_mixer_icon(p, pos2(rect.right() - 175.0, rect.center().y), 12.0, ORANGE);
    }
    // MIDI tag: white = MIDI file present, red = set but missing on disk
    if !s.midi_full.is_empty() {
        let c = if s.midi_exists { Color32::WHITE } else { RED };
        let r = Rect::from_center_size(pos2(rect.right() - 210.0, rect.center().y), vec2(30.0, 14.0));
        p.rect_stroke(r, 3.0, Stroke::new(1.0_f32, c));
        p.text(r.center(), Align2::CENTER_CENTER, "MIDI", FontId::proportional(9.0), c);
    }
    if s.warn() {
        paint_warn(p, rect);
    }
}

fn paint_mark(p: &egui::Painter, rect: Rect, mark: Mark) {
    let text = match mark {
        Mark::None => return,
        Mark::Skip => tr!("mark.skip"),
        Mark::Quick => tr!("mark.quick"),
        Mark::Overwrite => tr!("mark.overwrite"),
        Mark::New => tr!("mark.new"),
        Mark::Force => tr!("mark.force"),
    };
    // "Skip" (nothing is written) is grey, the others orange
    let color = if mark == Mark::Skip { DIM } else { ORANGE };
    let galley = p.layout_no_wrap(text.to_string(), FontId::proportional(10.5), color);
    let size = galley.size() + vec2(8.0, 3.0);
    let r = Rect::from_min_size(
        pos2(rect.right() - 6.0 - size.x, rect.center().y - size.y / 2.0),
        size,
    );
    match mark {
        Mark::New => {
            p.rect_filled(r, 2.0, Color32::WHITE);
        }
        Mark::Skip => {}
        _ => {
            p.rect_filled(r, 2.0, MARK_FILL);
        }
    }
    p.rect_stroke(r, 2.0, Stroke::new(1.0_f32, color));
    p.galley(r.min + vec2(4.0, 1.5), galley, color);
}

/// orange badge with white text ("Missing Wav file")
fn badge(ui: &mut Ui, text: &str) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), FontId::proportional(11.0), Color32::WHITE);
    let size = galley.size() + vec2(8.0, 4.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    ui.painter().rect_filled(rect, 2.0, ORANGE);
    ui.painter().galley(rect.min + vec2(4.0, 2.0), galley, Color32::WHITE);
    resp
}

/// small orange-framed button of the Details panels
pub(crate) fn small_btn(ui: &mut Ui, text: &str, enabled: bool) -> bool {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).size(11.5).color(TEXT))
            .fill(Color32::from_rgb(0x3a, 0x3a, 0x3a))
            .stroke(Stroke::new(1.0_f32, ORANGE))
            .rounding(3.0),
    )
    .clicked()
}

/// the small grey "tab" on top of a panel ("Project Explorer", "Details")
pub(crate) fn panel_tab(ui: &mut Ui, title: &str, refresh: bool) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        egui::Frame::none()
            .fill(PANEL)
            .inner_margin(Margin::symmetric(9.0, 4.0))
            .show(ui, |ui| {
                ui.label(RichText::new(title).size(12.5).color(TEXT));
            });
        if refresh
            && ui
                .add(egui::Button::new(RichText::new("🔄").size(13.0)).frame(false))
                .on_hover_text(tr!("common.refresh"))
                .clicked()
        {
            clicked = true;
        }
    });
    clicked
}

/// panel body (grey box under a panel tab); `fill` = take all space
pub(crate) fn panel_body<R>(ui: &mut Ui, fill: bool, body: impl FnOnce(&mut Ui) -> R) -> R {
    let spacing = ui.spacing().item_spacing.y;
    ui.add_space(-spacing);
    egui::Frame::none()
        .fill(PANEL)
        .inner_margin(Margin::same(8.0))
        .show(ui, |ui| {
            if fill {
                ui.set_min_size(ui.available_size());
            } else {
                ui.set_min_width(ui.available_width());
            }
            body(ui)
        })
        .inner
}

/// "Target Disk Location"-style label + grey field
fn field_box(ui: &mut Ui, title: &str, value: &str) {
    egui::Frame::none()
        .fill(Color32::from_rgb(0x52, 0x52, 0x52))
        .inner_margin(Margin::symmetric(5.0, 2.0))
        .show(ui, |ui| {
            ui.label(RichText::new(title).size(12.5).color(TEXT));
        });
    ui.add_space(-ui.spacing().item_spacing.y);
    egui::Frame::none()
        .fill(FIELD)
        .inner_margin(Margin::symmetric(5.0, 6.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new(value).color(TEXT));
        });
}

fn kv(ui: &mut Ui, k: &str, v: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{}:", k)).color(TEXT));
        ui.label(RichText::new(v).color(TEXT));
    });
}

fn list_box(ui: &mut Ui, body: impl FnOnce(&mut Ui)) {
    egui::Frame::none()
        .fill(LIST)
        .inner_margin(Margin::symmetric(16.0, 4.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            body(ui)
        });
}

/// uTool2 overlay: black backdrop, orange title, dark rounded box and a row
/// of evenly spaced orange-framed buttons below. Returns the clicked button.
pub(crate) fn overlay(
    ctx: &egui::Context,
    id: &str,
    order: egui::Order,
    title: &str,
    size: egui::Vec2,
    body: impl FnOnce(&mut Ui),
    buttons: &[(&str, bool)],
) -> Option<usize> {
    let screen = ctx.screen_rect();
    let mut clicked = None;
    egui::Area::new(egui::Id::new(id))
        .order(order)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            ui.painter().rect_filled(screen, 0.0, Color32::BLACK);
            ui.allocate_rect(screen, Sense::click_and_drag());
            let w = size.x.min(screen.width() - 60.0);
            let h = size.y.min(screen.height() - 150.0);
            let center = screen.center() - vec2(0.0, 10.0);
            let box_rect = Rect::from_center_size(center, vec2(w, h));
            ui.painter().text(
                pos2(center.x, box_rect.top() - 24.0),
                Align2::CENTER_CENTER,
                title,
                FontId::proportional(24.0),
                ORANGE,
            );
            ui.painter().rect_filled(box_rect, 8.0, Color32::from_rgb(0x33, 0x33, 0x33));
            ui.allocate_ui_at_rect(box_rect.shrink(18.0), |ui| {
                ui.set_clip_rect(box_rect.shrink(4.0));
                body(ui);
            });
            let n = buttons.len() as f32;
            let y = box_rect.bottom() + 26.0;
            for (i, (label, enabled)) in buttons.iter().enumerate() {
                let x = box_rect.left() + w * (i as f32 + 1.0) / (n + 1.0);
                let r = Rect::from_center_size(pos2(x, y), vec2(110.0f32.max(label.len() as f32 * 8.0), 28.0));
                let b = egui::Button::new(RichText::new(*label).color(TEXT))
                    .fill(Color32::from_rgb(0x2a, 0x2a, 0x2a))
                    .stroke(Stroke::new(1.0_f32, ORANGE))
                    .rounding(3.0);
                if ui.put(r, |ui: &mut Ui| ui.add_enabled(*enabled, b)).clicked() {
                    clicked = Some(i);
                }
            }
        });
    // the overlay drawn last (dialog over editor) must stay on top; never use
    // Order::Tooltip here: egui ignores all input on tooltip layers
    ctx.move_to_top(egui::LayerId::new(order, egui::Id::new(id)));
    clicked
}

fn centered_text(ui: &mut Ui, text: &str) {
    ui.with_layout(Layout::top_down(Align::Center), |ui| {
        ui.add_space((ui.available_height() / 2.0 - 30.0).max(0.0));
        ui.label(RichText::new(text).size(15.0).color(TEXT));
    });
}

fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = FIELD;
    v.faint_bg_color = PANEL;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = SEL;
    v.selection.stroke = Stroke::new(1.0_f32, TEXT);
    v.hyperlink_color = ORANGE;
    let r = egui::Rounding::same(3.0);
    v.widgets.inactive.bg_fill = Color32::from_rgb(0x3a, 0x3a, 0x3a);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x3a, 0x3a, 0x3a);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, ORANGE);
    v.widgets.inactive.rounding = r;
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x50, 0x50, 0x50);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x50, 0x50, 0x50);
    v.widgets.hovered.bg_stroke = Stroke::new(1.5_f32, ORANGE);
    v.widgets.hovered.rounding = r;
    v.widgets.active.bg_fill = Color32::from_rgb(0x5a, 0x5a, 0x5a);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(0x5a, 0x5a, 0x5a);
    v.widgets.active.bg_stroke = Stroke::new(1.5_f32, ORANGE);
    v.widgets.active.rounding = r;
    v.widgets.open.bg_stroke = Stroke::new(1.0_f32, ORANGE);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, LINE);
    v
}

// ---------------------------------------------------------------------------
// app: loading and actions
// ---------------------------------------------------------------------------

impl Lp16App {
    pub fn new(config: Config) -> Self {
        let mut open = HashSet::new();
        for k in ["h:root", "h:songs", "t:rec", "t:pl", "e:rec", "e:pl", "x:pl"] {
            open.insert(k.to_string());
        }
        let mut app = Lp16App {
            config,
            tab: Tab::Home,
            shown_tab: Tab::Home,
            songs: Vec::new(),
            song_idx: HashMap::new(),
            playlists: Vec::new(),
            sel: Sel::Root,
            open,
            drives: list_drives(),
            force: false,
            x_songs: HashSet::new(),
            x_playlists: HashSet::new(),
            preview: None,
            preview_gen: 0,
            export_job: None,
            folder_edit: false,
            et_sel: None,
            et_cache: None,
            et_rows: Vec::new(),
            et_scroll: false,
            dialog: None,
            pl_editor: None,
            song_ed: None,
            settings_dirty: false,
            import_scan: None,
            import_check: Vec::new(),
            import_show_all: false,
            pl_update: None,
            rehearsal: RehearsalPanel::default(),
            mix_saver: MixSaver::default(),
            backup: BackupPanel::default(),
            backup_saver: MixSaver::default(),
            bandpack: BandPack::default(),
            setlist_print: SetlistPrint::default(),
            audio: AudioPanel::default(),
            midi: MidiPanel::default(),
            tail_check: None,
            tail_queue: Vec::new(),
            tail_msg: None,
        };
        app.rehearsal.session.set_settings(AudioSettings::from(&app.config));
        app.rehearsal.session.set_midi_port(&app.config.midi_port);
        app.rehearsal.session.set_autoplay(app.config.rehearsal_autoplay);
        app.rehearsal.session.set_mix(app.config.mixes.active().params());
        app.refresh();
        app
    }

    /// Start waiting WAV-end checks, take over finished results and show
    /// their message once no other dialog is open.
    fn tail_check_tick(&mut self, ctx: &egui::Context) {
        if let Some((check, keys)) = &self.tail_check {
            if let Some(results) = check.take_finished() {
                let kind = check.kind;
                let keys: HashSet<String> = keys.iter().cloned().collect();
                let found: HashMap<String, Option<TailReport>> =
                    results.into_iter().map(|(w, r)| (key(&w), r)).collect();
                let mut affected = Vec::new();
                for s in self.songs.iter_mut() {
                    for t in s.tracks.iter_mut() {
                        if let Some(r) = found.get(&key(&t.wav)) {
                            t.tail = Some(*r);
                        }
                    }
                    if keys.contains(&key(&s.path)) && s.tracks.iter().any(|t| matches!(t.tail, Some(Some(_)))) {
                        affected.push(s.name.clone());
                    }
                }
                self.tail_msg = crate::gui_tail::summary(kind, keys.len(), found.len(), &affected);
                self.tail_check = None;
                if kind == TailCheckKind::Export {
                    if affected.is_empty() {
                        self.start_export(ctx);
                    } else {
                        let text = crate::gui_tail::export_question(keys.len(), &affected);
                        self.dialog = Some(Dialog::ExportTails(text));
                    }
                }
            }
        }
        if self.tail_check.is_none() && !self.tail_queue.is_empty() {
            let (kind, only) = self.tail_queue.remove(0);
            let songs: Vec<&SongInfo> = match &only {
                Some(keys) => self.songs.iter().filter(|s| keys.contains(&key(&s.path))).collect(),
                None => self.songs.iter().collect(),
            };
            let mut seen = HashSet::new();
            let wavs: Vec<String> = songs
                .iter()
                .flat_map(|s| s.tracks.iter())
                .filter(|t| t.exists && seen.insert(key(&t.wav)))
                .map(|t| t.wav.clone())
                .collect();
            let keys = songs.iter().map(|s| key(&s.path)).collect();
            self.tail_check = Some((TailCheck::start(kind, wavs, ctx), keys));
        }
        if self.dialog.is_none() && self.pl_update.is_none() && self.pl_editor.is_none() && self.song_ed.is_none() {
            if let Some(text) = self.tail_msg.take() {
                self.message(tr!("tail.title"), text);
            }
        }
    }

    /// Offer to repair the WAVs of song `index` that end without a fade-out.
    fn fix_tail_dialog(&mut self, index: usize) {
        let tracks: Vec<(String, String, TailReport)> = self.songs[index]
            .tracks
            .iter()
            .filter_map(|t| match &t.tail {
                Some(Some(r)) => Some((t.name.clone(), t.wav.clone(), *r)),
                _ => None,
            })
            .collect();
        let trim_ms = tracks.iter().map(|(_, _, r)| r.suggested_trim_ms).fold(0.0, f64::max);
        if !tracks.is_empty() {
            self.dialog = Some(Dialog::FixTail { song: index, tracks, trim_ms });
        }
    }

    /// Cut `trim_ms` off the WAVs and fade them out, then show the result.
    fn fix_tail_run(&mut self, song: usize, wavs: &[String], trim_ms: f64) {
        let path = self.songs[song].path.clone();
        // the player must not hold the old files
        self.rehearsal.session.song_changed(&path);
        let mut errors = Vec::new();
        for wav in wavs {
            if let Err(e) = wav_tail::fix_tail(wav, trim_ms, wav_tail::FADE_MS) {
                errors.push(e.to_string());
            }
        }
        let audio_lib = self.config.audio_lib.clone();
        let k = key(&path);
        let fixed: HashSet<String> = wavs.iter().map(|w| key(w)).collect();
        for s in self.songs.iter_mut() {
            // other songs may use the same WAVs: look at them again
            if key(&s.path) == k || s.tracks.iter().any(|t| fixed.contains(&key(&t.wav))) {
                let p = s.path.clone();
                *s = load_song(Path::new(&p), &audio_lib);
            }
        }
        let done = wavs.len() - errors.len();
        let mut text = if trim_ms > 0.0 {
            tr!("tail.cut_done", n = done, trim = format!("{:.0}", trim_ms), fade = format!("{:.0}", wav_tail::FADE_MS))
        } else {
            tr!("tail.fade_done", n = done, fade = format!("{:.0}", wav_tail::FADE_MS))
        };
        if done > 0 {
            text += tr!("tail.originals_kept");
        }
        if !errors.is_empty() {
            text += &tr!("common.failed", list = errors.join("\n"));
        }
        self.message(tr!("tail.fade_title"), text);
    }

    fn message(&mut self, title: &str, text: impl Into<String>) {
        self.dialog = Some(Dialog::Message {
            title: title.to_string(),
            text: text.into(),
        });
    }

    fn toggle(&mut self, k: &str) {
        if !self.open.remove(k) {
            self.open.insert(k.to_string());
        }
    }

    fn is_open(&self, k: &str) -> bool {
        self.open.contains(k)
    }

    /// The project library of the current settings.
    fn lib(&self) -> ProjectLibrary {
        ProjectLibrary::new(&self.config.project_lib, &self.config.audio_lib)
    }

    fn refresh(&mut self) {
        let lib = self.lib();
        let audio_lib = self.config.audio_lib.clone();
        // checked WAV endings stay known while the WAV is unchanged
        let known: HashMap<String, ((u64, i64), Option<TailReport>)> = self
            .songs
            .iter()
            .flat_map(|s| s.tracks.iter())
            .filter_map(|t| t.tail.map(|r| (key(&t.wav), (t.stamp, r))))
            .collect();
        self.songs = lib.song_files().iter().map(|p| load_song(p, &audio_lib)).collect();
        for t in self.songs.iter_mut().flat_map(|s| s.tracks.iter_mut()) {
            if let Some((stamp, r)) = known.get(&key(&t.wav)) {
                if *stamp == t.stamp {
                    t.tail = Some(*r);
                }
            }
        }
        self.song_idx = self
            .songs
            .iter()
            .enumerate()
            .map(|(i, s)| (key(&s.path), i))
            .collect();
        self.playlists = lib
            .playlists()
            .into_iter()
            .map(|pl| PlaylistInfo {
                name: pl.name,
                songs: pl
                    .songs
                    .iter()
                    .map(|s| PlSong { prj: s.songprj.clone(), mode: s.mode.clone(), delay: s.delay.clone() })
                    .collect(),
                markers: pl.markers,
                size: fs::metadata(&pl.path).map(|m| m.len()).unwrap_or(0),
                modified: mtime(&pl.path),
            })
            .collect();
        match self.sel {
            Sel::Song(i) if i >= self.songs.len() => self.sel = Sel::SongsFolder,
            Sel::Playlist(i) if i >= self.playlists.len() => self.sel = Sel::PlaylistsFolder,
            _ => {}
        }
        // the Rehearsal tab plays these playlists
        let for_player: Vec<_> = self.playlists.iter().map(|p| (p.name.clone(), to_pl_songs(&p.songs))).collect();
        self.backup.set_library(&audio_lib, for_player.clone());
        self.rehearsal.session.set_library(&audio_lib, for_player);
        self.rehearsal.set_markers(self.playlists.iter().map(|p| (p.name.clone(), p.markers)).collect());
        // "(single song)" lists all songs of the library
        let all_songs: Vec<PlaylistSong> = self
            .songs
            .iter()
            .map(|s| PlaylistSong {
                name: s.name.clone(),
                songprj: s.path.clone(),
                mode: lp16_tool::rehearsal_queue::WAIT_MODE.to_string(),
                delay: "0".into(),
            })
            .collect();
        self.rehearsal.session.set_library_songs(all_songs);
        self.x_songs.retain(|&i| i < self.songs.len());
        self.x_playlists.retain(|&i| i < self.playlists.len());
        self.drives = list_drives();
        self.preview_gen += 1;
        self.et_cache = None;
    }

    fn song_of(&self, prj: &str) -> Option<&SongInfo> {
        self.song_idx.get(&key(prj)).map(|&i| &self.songs[i])
    }

    fn playlist_warn(&self, i: usize) -> bool {
        self.playlists[i]
            .songs
            .iter()
            .any(|s| self.song_of(&s.prj).map(|x| x.warn()).unwrap_or(true))
    }

    fn playlist_len(&self, songs: &[PlSong]) -> f64 {
        songs
            .iter()
            .filter_map(|s| self.song_of(&s.prj))
            .map(|s| s.duration)
            .sum()
    }

    /// Start and end of the songs of a concert (missing songs count 0 s).
    fn concert_timeline(&self, songs: &[PlSong], markers: &SetMarkers, pause: u32) -> Timeline {
        let durations: Vec<f64> = songs.iter().map(|s| self.song_of(&s.prj).map(|x| x.duration).unwrap_or(0.0)).collect();
        setlist::timeline(&durations, markers, pause as f64)
    }

    fn select_song_path(&mut self, path: &Path) {
        if let Some(&i) = self.song_idx.get(&key(&path.to_string_lossy())) {
            self.sel = Sel::Song(i);
            self.open.insert("h:songs".into());
        }
    }

    fn select_playlist_name(&mut self, name: &str) {
        if let Some(i) = self.playlists.iter().position(|p| p.name == name) {
            self.sel = Sel::Playlist(i);
            self.open.insert("h:pl".into());
        }
    }

    /// Show a failed library change; true if it worked.
    fn lib_ok<T>(&mut self, result: Result<T, String>) -> bool {
        match result {
            Ok(_) => true,
            Err(e) => {
                self.message(tr!("common.error"), e);
                false
            }
        }
    }

    /// Rename (copy = false) or copy a song project to a new name; a rename
    /// keeps the playlists pointing at the song.
    fn song_name_change(&mut self, index: usize, new_name: &str, copy: bool) {
        let lib = self.lib();
        let old = PathBuf::from(&self.songs[index].path);
        if !copy {
            self.rehearsal.session.song_changed(&self.songs[index].path);
        }
        let result = if copy { lib.copy_song(&old, new_name) } else { lib.rename_song(&old, new_name) };
        let ok = self.lib_ok(result);
        self.refresh();
        if ok {
            self.select_song_path(&lib.song_path(new_name));
        }
    }

    fn playlist_name_change(&mut self, index: usize, new_name: &str, copy: bool) {
        let old = self.playlists[index].name.clone();
        let result = self.lib().rename_playlist(&old, new_name, copy);
        if self.lib_ok(result) {
            if !copy {
                self.rehearsal.session.playlist_renamed(&old, new_name);
            }
            self.refresh();
            self.select_playlist_name(new_name);
        }
    }

    fn delete_song(&mut self, index: usize) {
        self.rehearsal.session.song_changed(&self.songs[index].path);
        let path = PathBuf::from(&self.songs[index].path);
        match self.lib().delete_song(&path) {
            Ok(()) => {
                self.sel = Sel::SongsFolder;
                self.refresh();
            }
            Err(e) => self.message(tr!("common.delete_song"), e),
        }
    }

    fn delete_playlist(&mut self, index: usize) {
        let name = self.playlists[index].name.clone();
        let result = self.lib().delete_playlist(&name);
        if self.lib_ok(result) {
            self.rehearsal.session.playlist_deleted(&name);
            self.sel = Sel::PlaylistsFolder;
            self.refresh();
        }
    }

    fn open_playlist_editor(&mut self, index: Option<usize>) {
        let (name, items) = match index {
            Some(i) => (self.playlists[i].name.clone(), items_from(&self.playlists[i].songs, &self.playlists[i].markers)),
            None => (tr!("home.new_playlist_name").to_string(), Vec::new()),
        };
        self.pl_editor = Some(PlaylistEditor {
            index,
            name,
            checked: vec![false; items.len()],
            items,
            pause: self.config.set_pause,
            start_mode: StartMode::Button,
            start_delay: 5,
            dirty: index.is_none(),
            avail_open: true,
            avail_sel: None,
            anchor: None,
            drag: None,
        });
    }

    /// Save the editor (rename/copy decided by the caller).
    fn editor_save(&mut self, copy_or_rename: Option<bool>) {
        let Some(ed) = self.pl_editor.as_ref() else { return };
        let name = ed.name.trim().to_string();
        let (songs, markers) = split_items(&ed.items);
        let index = ed.index;
        if !valid_fat32_name(&name) {
            self.message(tr!("home.playlist_name"), tr!("home.playlist_name_invalid"));
            return;
        }
        let new_path = self.lib().playlist_path(&name);
        let same_name = index.map(|i| self.playlists[i].name == name).unwrap_or(false);
        if !same_name && new_path.exists() {
            self.message(tr!("home.playlist_name"), tr!("common.playlist_exists", name = name));
            return;
        }
        match (index, same_name, copy_or_rename) {
            (Some(_), false, None) => {
                // ask: rename or copy
                self.dialog = Some(Dialog::EditorSaveAs);
                return;
            }
            _ => {}
        }
        let lib = self.lib();
        let songs = to_pl_songs(&songs);
        let result = match (index, same_name, copy_or_rename) {
            (Some(i), false, Some(copy)) => lib.save_playlist_as(&self.playlists[i].name, &name, &songs, &markers, copy),
            (None, _, _) => lib.create_playlist(&name, &songs, &markers),
            _ => lib.save_playlist(&name, &songs, &markers),
        };
        if !self.lib_ok(result) {
            return;
        }
        if let (Some(i), false, Some(false)) = (index, same_name, copy_or_rename) {
            // saved under a new name and the old one removed: a rename
            let old = self.playlists[i].name.clone();
            self.rehearsal.session.playlist_renamed(&old, &name);
        }
        self.refresh();
        self.select_playlist_name(&name);
        let new_index = self.playlists.iter().position(|p| p.name == name);
        if let Some(ed) = self.pl_editor.as_mut() {
            ed.index = new_index;
            ed.dirty = false;
        }
    }

    fn set_target(&mut self, t: String) {
        if self.config.stick_root != t {
            self.config.stick_root = t;
            let _ = save_config(&config_path(), &self.config);
            self.preview_gen += 1;
            self.et_cache = None;
        }
    }

    fn target_display(&self) -> String {
        let t = &self.config.stick_root;
        if t.is_empty() {
            return String::new();
        }
        self.drives
            .iter()
            .find(|(root, _)| root.eq_ignore_ascii_case(t) || root.trim_end_matches('\\').eq_ignore_ascii_case(t))
            .map(|(_, d)| d.clone())
            .unwrap_or_else(|| t.clone())
    }

    /// songs selected for export: songs of checked playlists, then single songs
    fn export_selection(&self) -> (Vec<String>, Vec<usize>) {
        let mut seen = HashSet::new();
        let mut prjs = Vec::new();
        let mut pls: Vec<usize> = self.x_playlists.iter().copied().collect();
        pls.sort();
        for &i in &pls {
            for s in &self.playlists[i].songs {
                if seen.insert(key(&s.prj)) {
                    prjs.push(s.prj.clone());
                }
            }
        }
        let mut singles: Vec<usize> = self.x_songs.iter().copied().collect();
        singles.sort();
        for i in singles {
            let p = &self.songs[i].path;
            if seen.insert(key(p)) {
                prjs.push(p.clone());
            }
        }
        (prjs, pls)
    }

    fn ensure_preview(&mut self) {
        let (prjs, pls) = self.export_selection();
        let target = self.config.stick_root.clone();
        let k = format!("{:?}|{:?}|{}|{}|{}", prjs, pls, target, self.force, self.preview_gen);
        if self.preview.as_ref().map(|p| p.key == k).unwrap_or(false) {
            return;
        }
        let stick = sync::Stick::new(&target);
        let target_ok = !target.trim().is_empty() && Path::new(&target).is_dir();
        let selected: u64 = prjs.iter().filter_map(|p| self.song_of(p)).map(|s| s.size).sum();
        let mut to_copy = 0u64;
        let mut songs: Vec<(String, Mark)> = Vec::new();
        let mut plays: Vec<(String, Mark)> = Vec::new();
        if target_ok {
            let plan = sync::plan(
                &target,
                &self.config.audio_lib,
                &prjs,
                &self.export_playlists(&pls),
                self.export_mode(),
            );
            to_copy = plan.bytes_to_copy();
            let preview = stick.preview(&plan);
            songs = preview.songs.into_iter().map(|(n, a)| (n, a.map(song_mark).unwrap_or(Mark::None))).collect();
            plays = preview.playlists.into_iter().map(|(n, a)| (n, a.map(playlist_mark).unwrap_or(Mark::None))).collect();
        }
        self.preview = Some(Preview {
            key: k,
            target_ok,
            music: stick.has_music(),
            songs,
            plays,
            selected,
            to_copy,
            free: if target_ok { disk_free(&target) } else { None },
        });
    }

    fn export_mode(&self) -> ExportMode {
        if self.force { ExportMode::Force } else { ExportMode::Quick }
    }

    /// name and songs of the checked playlists (indices from export_selection)
    fn export_playlists(&self, pls: &[usize]) -> Vec<(String, Vec<PlaylistSong>)> {
        pls.iter()
            .map(|&i| (self.playlists[i].name.clone(), to_pl_songs(&self.playlists[i].songs)))
            .collect()
    }

    /// The Export button: check the endings of the songs to export first
    /// (`tail_check_tick` exports when they are clean, else asks).
    fn export_check(&mut self) {
        let target = self.config.stick_root.clone();
        if target.trim().is_empty() || !Path::new(&target).is_dir() {
            self.message(tr!("tab.export"), tr!("common.no_target"));
            return;
        }
        let (prjs, pls) = self.export_selection();
        if prjs.is_empty() && pls.is_empty() {
            self.message(tr!("tab.export"), tr!("export.nothing_selected"));
            return;
        }
        let keys = prjs.iter().map(|p| key(p)).collect();
        self.tail_queue.push((TailCheckKind::Export, Some(keys)));
    }

    fn start_export(&mut self, ctx: &egui::Context) {
        let target = self.config.stick_root.clone();
        if target.trim().is_empty() || !Path::new(&target).is_dir() {
            self.message(tr!("tab.export"), tr!("common.no_target"));
            return;
        }
        let (prjs, pls) = self.export_selection();
        if prjs.is_empty() && pls.is_empty() {
            self.message(tr!("tab.export"), tr!("export.nothing_selected"));
            return;
        }
        let playlists = self.export_playlists(&pls);
        let job = Arc::new(Mutex::new(ExportJob {
            total: prjs.len() + playlists.len(),
            done: 0,
            current: String::new(),
            lines: Vec::new(),
            finished: false,
            cancel: false,
            started: Instant::now(),
            elapsed: 0.0,
        }));
        self.export_job = Some(job.clone());
        let config = self.config.clone();
        let mode = self.export_mode();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let plan = sync::plan(&target, &config.audio_lib, &prjs, &playlists, mode);
            job.lock().unwrap().total = plan.jobs();
            let result = sync::run(&plan, |e| {
                let mut j = job.lock().unwrap();
                match e {
                    Progress::Start(name) => j.current = name.to_string(),
                    Progress::Done(line) => {
                        let what = match line.kind {
                            sync::Kind::Song => tr!("export.kind_song"),
                            sync::Kind::Playlist => tr!("export.kind_playlist"),
                        };
                        j.lines.push((format!("{} {}", what, line.name), line.ok, line.message.clone()));
                        j.done += 1;
                    }
                }
                let go_on = !j.cancel;
                drop(j);
                ctx.request_repaint();
                go_on
            });
            let mut j = job.lock().unwrap();
            if let Err(e) = result {
                j.lines.push((tr!("tab.export").to_string(), false, tr!("export.error_line", err = e)));
            }
            j.elapsed = j.started.elapsed().as_secs_f64();
            j.finished = true;
            drop(j);
            ctx.request_repaint();
        });
    }

    fn init_folders(&mut self) {
        // pending edits in the fields count
        if self.settings_dirty {
            self.save_settings();
        }
        match init_folders(&self.config) {
            Ok(c) if c.is_empty() => self.message(tr!("settings.init_folders"), tr!("settings.init_all_exist")),
            Ok(c) => {
                self.refresh();
                self.message(tr!("settings.init_folders"), tr!("settings.init_created", list = c.join("\n")));
            }
            Err(e) => self.message(tr!("settings.init_folders"), tr!("settings.init_failed", err = e)),
        }
    }

    fn save_settings(&mut self) {
        self.config.apply_library();
        // other library: the old import scan does not fit any more
        self.import_scan = None;
        if let Err(e) = save_config(&config_path(), &self.config) {
            self.message(tr!("common.settings"), tr!("common.cannot_save", path = config_path().display(), err = e));
            return;
        }
        self.settings_dirty = false;
        self.refresh();
    }
}

// ---------------------------------------------------------------------------
// UI: frame, tab bar
// ---------------------------------------------------------------------------

impl eframe::App for Lp16App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("tabs")
            .frame(egui::Frame::none().fill(BG).inner_margin(Margin {
                left: 8.0,
                right: 10.0,
                top: 0.0,
                bottom: 4.0,
            }))
            .show_separator_line(false)
            .show(ctx, |ui| self.tab_bar(ui));
        // coming to Home reads the library again
        if self.tab == Tab::Home && self.shown_tab != Tab::Home {
            self.refresh();
        }
        self.shown_tab = self.tab;
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG).inner_margin(Margin::same(10.0)))
            .show(ctx, |ui| match self.tab {
                Tab::Home => self.home(ui),
                Tab::Rehearsal => {
                    if self.rehearsal.ui(ui, &mut self.config.mixes) {
                        self.mix_saver.mark();
                    }
                }
                Tab::Export => self.export(ui),
                Tab::Backup => {
                    if self.backup.ui(ui, &mut self.config.backup, &mut self.rehearsal) {
                        self.backup_saver.mark();
                    }
                }
                Tab::Import => self.import(ui),
                Tab::EditTarget => self.edit_target(ui),
                Tab::Settings => self.settings(ui),
                Tab::Support => self.support(ui),
            });
        // while the backup tab is shown the player plays its mix
        let stereo = (self.tab == Tab::Backup).then(|| self.backup.preview(&self.config.backup.mix));
        self.rehearsal.session.set_stereo(stereo);
        // a changed song mix is saved a moment after the last change; the
        // lists show the mixer symbol from the file
        self.rehearsal.tick(ctx);
        for path in self.rehearsal.session.take_saved_songs() {
            let k = key(&path);
            let audio_lib = self.config.audio_lib.clone();
            for s in self.songs.iter_mut().filter(|s| key(&s.path) == k) {
                *s = load_song(Path::new(&path), &audio_lib);
            }
        }
        self.tail_check_tick(ctx);
        // the autoplay checkbox of the Rehearsal tab
        if let Some(on) = self.rehearsal.take_autoplay_change() {
            self.config.rehearsal_autoplay = on;
            if let Err(e) = save_config(&config_path(), &self.config) {
                self.message(tr!("common.settings"), tr!("common.cannot_save", path = config_path().display(), err = e));
            }
        }
        self.playlist_editor(ctx);
        self.playlist_update(ctx);
        self.song_editor(ctx, frame);
        self.export_overlay(ctx);
        let lib = self.lib();
        if let Some(playlist) = self.bandpack.ui(ctx, &lib) {
            self.refresh();
            if !playlist.is_empty() {
                self.tab = Tab::Home;
                self.select_playlist_name(&playlist);
            }
        }
        let printed = self.setlist_print.ui(ctx, &lib, &self.config.setlist_logo);
        if let Some(logo) = printed.logo {
            self.config.setlist_logo = logo;
            if let Err(e) = save_setlist_logo(&config_path(), &self.config.setlist_logo) {
                self.message(tr!("common.settings"), tr!("common.cannot_save", path = config_path().display(), err = e));
            }
        }
        if printed.titles_changed {
            self.refresh();
        }
        self.dialogs(ctx);
        // rehearsal mix: saved at most once a second, bundled
        let now = std::time::Instant::now();
        if self.mix_saver.poll(now) {
            let _ = save_mixes(&config_path(), &self.config.mixes);
        }
        if let Some(wait) = self.mix_saver.wait(now) {
            ctx.request_repaint_after(wait);
        }
        // the backup export settings, the same way
        if self.backup_saver.poll(now) {
            let _ = save_backup(&config_path(), &self.config.backup);
        }
        if let Some(wait) = self.backup_saver.wait(now) {
            ctx.request_repaint_after(wait);
        }
        // the position display moves while a song plays
        if self.rehearsal.session.is_playing() {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }
    }

    /// eframe stores the window size and position, the UI zoom (Ctrl +/-)
    /// and the panel sizes (egui memory) and restores them at the next
    /// start. It writes the file only when something changed, so a short
    /// interval costs nothing while nothing moves.
    fn auto_save_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(1)
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // the rehearsal mix is always saved when the program ends
        let _ = save_mixes(&config_path(), &self.config.mixes);
        let _ = save_backup(&config_path(), &self.config.backup);
        self.rehearsal.session.shutdown();
    }
}

impl Lp16App {
    fn tab_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (t, title) in tabs() {
                // long titles get a wider tab
                let w = ui.fonts(|f| f.layout_no_wrap(title.to_string(), FontId::proportional(14.0), Color32::WHITE).size().x);
                let (rect, resp) = ui.allocate_exact_size(vec2((w + 20.0).max(78.0), 32.0), Sense::click());
                let fill = if self.tab == t {
                    ORANGE
                } else if resp.hovered() {
                    TAB_HOVER
                } else {
                    TAB_IDLE
                };
                ui.painter().rect_filled(rect, 0.0, fill);
                ui.painter().text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    title,
                    FontId::proportional(14.0),
                    Color32::WHITE,
                );
                if resp.clicked() {
                    self.tab = t;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(concat!("v", env!("CARGO_PKG_VERSION"))).size(11.0).color(ORANGE));
                ui.label(RichText::new("lp16-tool").size(28.0).color(ORANGE));
            });
        });
    }

    /// two equal panels side by side, filling the rest of the page
    fn two_panels(&mut self, ui: &mut Ui, height: f32, left: impl FnOnce(&mut Self, &mut Ui), right: impl FnOnce(&mut Self, &mut Ui)) {
        let w = ui.available_width();
        let gap = 10.0;
        let half = (w - gap) / 2.0;
        let top = ui.cursor().min;
        let lrect = Rect::from_min_size(top, vec2(half, height));
        let rrect = Rect::from_min_size(top + vec2(half + gap, 0.0), vec2(half, height));
        ui.allocate_ui_at_rect(lrect, |ui| left(self, ui));
        ui.allocate_ui_at_rect(rrect, |ui| right(self, ui));
        ui.allocate_rect(Rect::from_min_size(top, vec2(w, height)), Sense::hover());
    }

    // -----------------------------------------------------------------------
    // Home
    // -----------------------------------------------------------------------

    fn home(&mut self, ui: &mut Ui) {
        let h = ui.available_height();
        self.two_panels(
            ui,
            h,
            |app, ui| {
                if panel_tab(ui, tr!("common.project_explorer"), true) {
                    app.refresh();
                }
                panel_body(ui, true, |ui| app.home_tree(ui));
            },
            |app, ui| {
                panel_tab(ui, tr!("common.details"), false);
                panel_body(ui, true, |ui| {
                    egui::ScrollArea::vertical()
                        .id_source("home_details")
                        .auto_shrink([false; 2])
                        .show(ui, |ui| app.home_details(ui));
                });
            },
        );
    }

    fn home_tree(&mut self, ui: &mut Ui) {
        let songs_warn = self.songs.iter().any(|s| s.warn());
        let pl_warn = (0..self.playlists.len()).any(|i| self.playlist_warn(i));
        egui::ScrollArea::vertical()
            .id_source("home_tree")
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let open_root = self.is_open("h:root");
                let r = tree_row(ui, 0, Some(open_root), None, "", "LP-16", self.sel == Sel::Root, false, |p, rect| {
                    if songs_warn || pl_warn {
                        paint_warn(p, rect)
                    }
                });
                if r.toggle {
                    self.toggle("h:root");
                } else if r.clicked {
                    self.sel = Sel::Root;
                }
                if open_root {
                    // Multitrack Songs
                    let open = self.is_open("h:songs");
                    let r = tree_row(ui, 1, Some(open), None, "", tr!("common.multitrack_songs"), self.sel == Sel::SongsFolder, false, |p, rect| {
                        if songs_warn {
                            paint_warn(p, rect)
                        }
                    });
                    let mut new_song = false;
                    r.resp.context_menu(|ui| {
                        if ui.button(tr!("common.new_ml_song")).clicked() {
                            new_song = true;
                            ui.close_menu();
                        }
                    });
                    if new_song {
                        self.sel = Sel::SongsFolder;
                        self.new_song_dialog();
                    } else if r.toggle {
                        self.toggle("h:songs");
                    } else if r.clicked {
                        self.sel = Sel::SongsFolder;
                    }
                    if open {
                        for i in 0..self.songs.len() {
                            let s = &self.songs[i];
                            let r = tree_row(ui, 2, None, None, "♫", &s.name, self.sel == Sel::Song(i), true, |p, rect| {
                                paint_song_cols(p, rect, s)
                            });
                            let mut play = false;
                            r.resp.context_menu(|ui| {
                                if ui.button(tr!("common.play_in_rehearsal")).clicked() {
                                    play = true;
                                    ui.close_menu();
                                }
                            });
                            if play {
                                self.sel = Sel::Song(i);
                                self.play_in_rehearsal(i);
                            } else if r.double {
                                self.open_song_editor(i);
                            } else if r.clicked {
                                self.sel = Sel::Song(i);
                            }
                        }
                    }
                    // Multitrack Playlists
                    let open = self.is_open("h:pl");
                    let r = tree_row(ui, 1, Some(open), None, "", tr!("common.multitrack_playlists"), self.sel == Sel::PlaylistsFolder, false, |p, rect| {
                        if pl_warn {
                            paint_warn(p, rect)
                        }
                    });
                    let mut new_pl = false;
                    r.resp.context_menu(|ui| {
                        if ui.button(tr!("common.new_ml_playlist")).clicked() {
                            new_pl = true;
                            ui.close_menu();
                        }
                    });
                    if new_pl {
                        self.sel = Sel::PlaylistsFolder;
                        self.open_playlist_editor(None);
                    } else if r.toggle {
                        self.toggle("h:pl");
                    } else if r.clicked {
                        self.sel = Sel::PlaylistsFolder;
                    }
                    if open {
                        for i in 0..self.playlists.len() {
                            let warn = self.playlist_warn(i);
                            let len = fmt_len(self.playlist_len(&self.playlists[i].songs));
                            let r = tree_row(ui, 2, None, None, "☰", &self.playlists[i].name, self.sel == Sel::Playlist(i), true, |p, rect| {
                                p.text(pos2(rect.right() - 30.0, rect.center().y), Align2::RIGHT_CENTER, &len, FontId::proportional(11.5), TEXT);
                                if warn {
                                    paint_warn(p, rect)
                                }
                            });
                            let mut play = false;
                            r.resp.context_menu(|ui| {
                                if ui.button(tr!("common.play_in_rehearsal")).clicked() {
                                    play = true;
                                    ui.close_menu();
                                }
                            });
                            if play {
                                self.sel = Sel::Playlist(i);
                                self.play_playlist_in_rehearsal(i);
                            } else if r.double {
                                self.open_playlist_editor(Some(i));
                            } else if r.clicked {
                                self.sel = Sel::Playlist(i);
                            }
                        }
                    }
                }
            });
    }

    fn home_details(&mut self, ui: &mut Ui) {
        let title = |ui: &mut Ui, t: &str| {
            ui.vertical_centered(|ui| {
                ui.add_space(4.0);
                ui.label(RichText::new(t).size(18.0).strong().color(TEXT));
            });
            ui.add_space(4.0);
        };
        match self.sel {
            Sel::Root => {
                title(ui, "LP-16");
            }
            Sel::SongsFolder => {
                title(ui, tr!("common.multitrack_songs"));
                let (mut new, mut refresh, mut check) = (false, false, false);
                let running = self.tail_check.as_ref().map(|(c, _)| c.progress());
                ui.horizontal(|ui| {
                    new = small_btn(ui, tr!("common.new_ml_song"), true);
                    refresh = small_btn(ui, tr!("common.refresh"), true);
                    check = small_btn(ui, tr!("home.check_endings"), running.is_none());
                    if let Some((done, total)) = running {
                        ui.label(tr!("common.checking_endings", done = done, total = total));
                    }
                });
                ui.add_space(8.0);
                field_box(ui, tr!("common.target_disk_location"), &stick_location(""));
                ui.add_space(14.0);
                let mut srs: Vec<u32> = self.songs.iter().map(|s| s.sr).collect();
                srs.sort();
                srs.dedup();
                let mut bits: Vec<u32> = self.songs.iter().map(|s| s.bits).collect();
                bits.sort();
                bits.dedup();
                ui.label(tr!("home.branch_songs", n = self.songs.len()));
                ui.label(tr!("home.rates_in_use", list = srs.iter().map(|&s| fmt_sr(s)).collect::<Vec<_>>().join(", ")));
                ui.label(tr!("home.bits_in_use", list = bits.iter().map(|b| tr!("common.bits", n = b)).collect::<Vec<_>>().join(", ")));
                if new {
                    self.new_song_dialog();
                }
                if refresh {
                    self.refresh();
                }
                if check {
                    self.tail_queue.push((TailCheckKind::Library, None));
                }
            }
            Sel::PlaylistsFolder => {
                title(ui, tr!("common.multitrack_playlists"));
                let (mut new, mut refresh, mut pack) = (false, false, false);
                ui.horizontal(|ui| {
                    new = small_btn(ui, tr!("common.new_ml_playlist"), true);
                    refresh = small_btn(ui, tr!("common.refresh"), true);
                    pack = small_btn(ui, tr!("pack.export_btn"), !self.playlists.is_empty());
                });
                ui.add_space(8.0);
                field_box(ui, tr!("common.target_disk_location"), &stick_location(sync::PLAYLISTS));
                ui.add_space(14.0);
                ui.label(tr!("home.branch_playlists", n = self.playlists.len()));
                if new {
                    self.open_playlist_editor(None);
                }
                if refresh {
                    self.refresh();
                }
                if pack {
                    self.bandpack.choose(self.playlists.iter().map(|p| p.name.clone()).collect(), None);
                }
            }
            Sel::Song(i) => {
                let mut act = 0;
                // look at the WAV ends once (the last 200 ms of each track)
                for t in self.songs[i].tracks.iter_mut().filter(|t| t.exists && t.tail.is_none()) {
                    t.tail = Some(wav_tail::analyze_tail(&t.wav).ok().flatten());
                }
                {
                    let s = &self.songs[i];
                    title(ui, &s.name);
                    ui.horizontal(|ui| {
                        small_btn(ui, tr!("common.new_ml_song"), false);
                        if small_btn(ui, tr!("common.edit"), true) {
                            act = 1;
                        }
                        if small_btn(ui, tr!("common.mixer"), true) {
                            act = 5;
                        }
                        if small_btn(ui, tr!("common.rename"), true) {
                            act = 2;
                        }
                        if small_btn(ui, tr!("common.delete"), true) {
                            act = 3;
                        }
                        if small_btn(ui, tr!("common.duplicate"), true) {
                            act = 4;
                        }
                    });
                    ui.add_space(8.0);
                    field_box(ui, tr!("common.target_disk_location"), &stick_location(&s.name));
                    ui.add_space(14.0);
                    kv(ui, tr!("common.type"), tr!("home.type_song"));
                    let print = if s.print_title.is_empty() {
                        tr!("home.print_title_unset", name = song_info::default_print_title(&s.name))
                    } else {
                        s.print_title.clone()
                    };
                    kv(ui, tr!("common.print_title"), &print);
                    kv(ui, tr!("common.sample_rate"), &fmt_sr(s.sr));
                    kv(ui, tr!("common.resolution"), &tr!("common.bits", n = s.bits));
                    kv(ui, tr!("home.track_count"), &s.tracks.len().to_string());
                    ui.label(tr!("home.tracks"));
                    list_box(ui, |ui| {
                        for t in &s.tracks {
                            ui.horizontal(|ui| {
                                ui.label(format!("{:>3}: {}", t.num, t.name));
                                if !t.exists {
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        badge(ui, tr!("common.missing_wav"))
                                            .on_hover_text(tr!("home.expected_wav", path = t.wav));
                                    });
                                } else if let Some(Some(r)) = &t.tail {
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        let b = badge(ui, tr!("tail.no_clean"))
                                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                                            .on_hover_text(tr!("home.click_fade", what = r.describe()));
                                        if b.clicked() {
                                            act = 6;
                                        }
                                    });
                                }
                            });
                        }
                    });
                    kv(ui, tr!("common.length"), &fmt_len(s.duration));
                    kv(ui, tr!("common.size"), &fmt_size(s.size));
                    kv(ui, tr!("common.mod_date"), &fmt_date(s.modified));
                    ui.horizontal(|ui| {
                        ui.label(tr!("home.midi_file", name = if s.midi_name.is_empty() { "-" } else { &s.midi_name }));
                        if !s.midi_full.is_empty() && !s.midi_exists {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                badge(ui, tr!("common.missing_midi"))
                                    .on_hover_text(tr!("home.expected_midi", path = s.midi_full));
                            });
                        }
                    });
                    ui.label(tr!("common.related_playlists"));
                    let k = key(&s.path);
                    let related: Vec<&str> = self
                        .playlists
                        .iter()
                        .filter(|pl| pl.songs.iter().any(|x| key(&x.prj) == k))
                        .map(|pl| pl.name.as_str())
                        .collect();
                    list_box(ui, |ui| {
                        for n in related {
                            ui.label(format!("    {}", n));
                        }
                    });
                }
                match act {
                    1 => self.open_song_editor(i),
                    2 => {
                        self.dialog = Some(Dialog::SongName {
                            index: i,
                            name: self.songs[i].name.clone(),
                            copy_only: false,
                        })
                    }
                    3 => self.dialog = Some(Dialog::DeleteSong(i)),
                    4 => {
                        self.dialog = Some(Dialog::SongName {
                            index: i,
                            name: format!("{}_copy", self.songs[i].name),
                            copy_only: true,
                        })
                    }
                    5 => self.open_in_mixer(i),
                    6 => self.fix_tail_dialog(i),
                    _ => {}
                }
            }
            Sel::Playlist(i) => {
                let mut act = 0;
                {
                    let pl = &self.playlists[i];
                    title(ui, &pl.name);
                    ui.horizontal(|ui| {
                        if small_btn(ui, tr!("common.new_ml_playlist"), true) {
                            act = 5;
                        }
                        if small_btn(ui, tr!("common.edit"), true) {
                            act = 1;
                        }
                        if small_btn(ui, tr!("common.rename"), true) {
                            act = 2;
                        }
                        if small_btn(ui, tr!("common.delete"), true) {
                            act = 3;
                        }
                        if small_btn(ui, tr!("common.duplicate"), true) {
                            act = 4;
                        }
                        if small_btn(ui, tr!("home.update_songs"), true) {
                            act = 6;
                        }
                        if small_btn(ui, tr!("pack.export_btn"), !pl.songs.is_empty()) {
                            act = 7;
                        }
                        if small_btn(ui, tr!("print.btn"), !pl.songs.is_empty()) {
                            act = 8;
                        }
                    });
                    ui.add_space(8.0);
                    field_box(ui, tr!("common.target_disk_location"), &stick_location(sync::PLAYLISTS));
                    ui.add_space(14.0);
                    kv(ui, tr!("common.type"), tr!("home.type_playlist"));
                    kv(ui, tr!("common.song_count"), &pl.songs.len().to_string());
                    let tl = self.concert_timeline(&pl.songs, &pl.markers, self.config.set_pause);
                    let times_font = FontId::proportional(11.5);
                    list_box(ui, |ui| {
                        // number of songs before the current row
                        let mut n = 0usize;
                        for item in items_from(&pl.songs, &pl.markers) {
                            match item {
                                PlItem::Song(s) => {
                                    let info = self.song_of(&s.prj);
                                    let times = tl.rows.get(n).copied().flatten();
                                    // after the end of the concert: for information only
                                    let color = if times.is_some() { TEXT } else { DIM };
                                    n += 1;
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(format!("{:>3}: {}", n, stem(&s.prj))).color(color));
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            mode_widget(ui, &s.mode, &s.delay);
                                            if let Some((start, end)) = times {
                                                time_col(ui, &fmt_clock(end), TEXT);
                                                ui.label(RichText::new("–").font(times_font.clone()).color(DIM));
                                                time_col(ui, &fmt_clock(start), TEXT);
                                            }
                                            match info {
                                                None => {
                                                    badge(ui, tr!("common.missing_song")).on_hover_text(&s.prj);
                                                }
                                                Some(x) if x.warn() => {
                                                    badge(ui, tr!("home.missing_files"));
                                                }
                                                _ => {}
                                            }
                                        });
                                    });
                                }
                                marker => {
                                    // a marker row, tinted like in the playlist editor
                                    let (text, time) = match marker {
                                        PlItem::Encore => {
                                            let t = if tl.main_set.is_some() {
                                                tr!("common.encores_n", n = tl.encore_count)
                                            } else {
                                                tr!("common.encores_after").into()
                                            };
                                            (t, tl.main_set)
                                        }
                                        _ => (tr!("common.end_of_concert").to_string(), Some(tl.concert)),
                                    };
                                    egui::Frame::none().fill(MARKER_FILL).outer_margin(Margin::symmetric(-16.0, 0.0)).inner_margin(Margin::symmetric(16.0, 2.0)).show(ui, |ui| {
                                        ui.set_min_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            ui.label(RichText::new(format!("       {}", text)).color(ORANGE));
                                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                                // the width of the start mode icon of the song rows
                                                ui.add_space(16.0 + ui.spacing().item_spacing.x);
                                                if let Some(t) = time {
                                                    time_col(ui, &fmt_clock(t), ORANGE);
                                                }
                                            });
                                        });
                                    });
                                }
                            }
                        }
                    });
                    kv(ui, tr!("common.length"), &fmt_len(self.playlist_len(&pl.songs)));
                    let concert = match tl.main_set {
                        Some(main) => tr!("home.concert_main", total = fmt_clock(tl.concert), main = fmt_clock(main), n = tl.encore_count, pause = self.config.set_pause),
                        None => tr!("home.concert", total = fmt_clock(tl.concert), pause = self.config.set_pause),
                    };
                    kv(ui, tr!("home.concert_label"), &concert);
                    kv(ui, tr!("common.size"), &fmt_size(pl.size));
                    kv(ui, tr!("common.mod_date"), &fmt_date(pl.modified));
                }
                match act {
                    1 => self.open_playlist_editor(Some(i)),
                    2 => {
                        self.dialog = Some(Dialog::PlaylistName {
                            index: i,
                            name: self.playlists[i].name.clone(),
                            copy_only: false,
                        })
                    }
                    3 => self.dialog = Some(Dialog::DeletePlaylist(i)),
                    4 => {
                        self.dialog = Some(Dialog::PlaylistName {
                            index: i,
                            name: format!("{}_copy", self.playlists[i].name),
                            copy_only: true,
                        })
                    }
                    5 => self.open_playlist_editor(None),
                    6 => {
                        self.open_playlist_update(Some(i), &[], "");
                    }
                    7 => {
                        let names = self.playlists.iter().map(|p| p.name.clone()).collect();
                        self.bandpack.choose(names, Some(&self.playlists[i].name));
                    }
                    8 => {
                        let pl = &self.playlists[i];
                        let songs: Vec<String> = pl.songs.iter().map(|s| s.prj.clone()).collect();
                        self.setlist_print.open(&pl.name, &songs, pl.markers);
                    }
                    _ => {}
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Export
    // -----------------------------------------------------------------------

    fn target_picker(&mut self, ui: &mut Ui, width: f32) {
        let mut new_target: Option<String> = None;
        ui.label(RichText::new("🗀").size(15.0)).on_hover_text(tr!("export.target_location"));
        if ui
            .add(egui::Button::new(RichText::new("✏").size(13.0)).frame(false))
            .on_hover_text(tr!("export.type_folder"))
            .clicked()
        {
            self.folder_edit = !self.folder_edit;
        }
        if self.folder_edit {
            let mut t = self.config.stick_root.clone();
            let resp = ui.add(egui::TextEdit::singleline(&mut t).desired_width(width));
            if resp.changed() {
                self.config.stick_root = t;
            }
            if resp.lost_focus() {
                let t = self.config.stick_root.clone();
                self.config.stick_root.clear();
                new_target = Some(t);
                self.folder_edit = false;
            }
        } else {
            let shown = self.target_display();
            egui::ComboBox::from_id_source("target")
                .width(width)
                .selected_text(if shown.is_empty() { tr!("export.select_drive").to_string() } else { shown })
                .show_ui(ui, |ui| {
                    for (root, display) in &self.drives {
                        if ui.selectable_label(self.config.stick_root.eq_ignore_ascii_case(root), display).clicked() {
                            new_target = Some(root.clone());
                        }
                    }
                });
        }
        if let Some(t) = new_target {
            self.set_target(t);
        }
    }

    fn export(&mut self, ui: &mut Ui) {
        self.ensure_preview();
        panel_tab(ui, tr!("common.settings"), false);
        panel_body(ui, false, |ui| {
            ui.horizontal(|ui| {
                ui.label(tr!("export.target_type"));
                egui::ComboBox::from_id_source("target_type")
                    .width(ui.available_width() * 0.42)
                    .selected_text("LP-16")
                    .show_ui(ui, |ui| {
                        let _ = ui.selectable_label(true, "LP-16");
                    });
                ui.add_space(20.0);
                ui.label(tr!("export.target"));
                let w = ui.available_width() - 60.0;
                self.target_picker(ui, w);
            });
            let p = self.preview.as_ref().unwrap();
            let free_after = p.free.map(|f| f.saturating_sub(p.to_copy));
            let (sel_t, copy_t, free_t, after_t) = (
                if p.selected > 0 { fmt_size(p.selected) } else { "-".into() },
                if p.target_ok && p.selected > 0 { fmt_size(p.to_copy) } else { "-".into() },
                p.free.map(fmt_size).unwrap_or_else(|| "-".into()),
                free_after.map(fmt_size).unwrap_or_else(|| "-".into()),
            );
            let mut force = self.force;
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.label(tr!("export.selected"));
                ui.label(sel_t);
                ui.add_space(120.0);
                ui.label(tr!("export.to_copy"));
                ui.label(copy_t);
                ui.add_space(120.0);
                ui.label(tr!("export.free"));
                ui.label(RichText::new(free_t).color(GREEN));
                ui.add_space(120.0);
                ui.label(tr!("export.free_after"));
                ui.label(RichText::new(after_t).color(GREEN));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    egui::ComboBox::from_id_source("mode")
                        .width(130.0)
                        .selected_text(if force { tr!("mark.force") } else { tr!("mark.quick") })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut force, false, tr!("mark.quick"))
                                .on_hover_text(tr!("export.quick_tip"));
                            ui.selectable_value(&mut force, true, tr!("mark.force"))
                                .on_hover_text(tr!("export.force_tip"));
                        });
                });
            });
            self.force = force;
        });
        ui.add_space(10.0);
        let h = ui.available_height() - 46.0;
        self.two_panels(
            ui,
            h,
            |app, ui| {
                if panel_tab(ui, tr!("common.project_explorer"), true) {
                    app.refresh();
                }
                panel_body(ui, true, |ui| app.export_tree(ui));
            },
            |app, ui| {
                if panel_tab(ui, tr!("export.target_content"), true) {
                    app.preview_gen += 1;
                    app.drives = list_drives();
                }
                panel_body(ui, true, |ui| app.target_content(ui));
            },
        );
        ui.add_space(6.0);
        // the check of the song endings before an export (queued or running)
        let checking = self.tail_queue.iter().any(|(k, _)| *k == TailCheckKind::Export)
            || self.tail_check.as_ref().map(|(c, _)| c.kind == TailCheckKind::Export).unwrap_or(false);
        let can_export = (!self.x_songs.is_empty() || !self.x_playlists.is_empty()) && !checking;
        let mut go = false;
        ui.horizontal(|ui| {
            let w = ui.available_width();
            ui.add_space(w / 2.0 - 50.0);
            go = ui
                .add_enabled(
                    can_export,
                    egui::Button::new(RichText::new(tr!("tab.export")).color(TEXT)).min_size(vec2(100.0, 28.0)),
                )
                .clicked();
            if checking {
                let text = match self.tail_check.as_ref().filter(|(c, _)| c.kind == TailCheckKind::Export) {
                    Some((c, _)) => {
                        let (done, total) = c.progress();
                        tr!("common.checking_endings", done = done, total = total)
                    }
                    None => tr!("export.checking").to_string(),
                };
                ui.label(RichText::new(text).color(DIM));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled(false, egui::Button::new(tr!("export.batch")).min_size(vec2(110.0, 28.0)));
            });
        });
        if go {
            self.export_check();
        }
    }

    fn export_tree(&mut self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(tr!("export.review")).size(11.0).color(DIM));
        });
        let songs_warn = self.songs.iter().any(|s| s.warn());
        let pl_warn: Vec<bool> = (0..self.playlists.len()).map(|i| self.playlist_warn(i)).collect();
        let folder_check = |sel: &HashSet<usize>, n: usize| {
            if sel.is_empty() {
                Check::Off
            } else if sel.len() == n {
                Check::On
            } else {
                Check::Partial
            }
        };
        egui::ScrollArea::vertical()
            .id_source("export_tree")
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                // songs
                let open = self.is_open("x:songs");
                let ok_songs: Vec<usize> = (0..self.songs.len()).filter(|&i| !self.songs[i].warn()).collect();
                // songs of checked playlists are exported anyway: shown checked, greyed out
                let via_pl: HashSet<usize> = self
                    .x_playlists
                    .iter()
                    .flat_map(|&p| self.playlists[p].songs.iter())
                    .filter_map(|s| self.song_idx.get(&key(&s.prj)).copied())
                    .collect();
                let shown: HashSet<usize> = self.x_songs.union(&via_pl).copied().collect();
                let c = folder_check(&shown, ok_songs.len().max(1));
                let r = tree_row(ui, 0, Some(open), Some((c, true)), "", tr!("common.multitrack_songs"), false, false, |p, rect| {
                    if songs_warn {
                        paint_warn(p, rect)
                    }
                });
                if r.toggle {
                    self.toggle("x:songs");
                }
                if r.check {
                    if self.x_songs.is_empty() {
                        self.x_songs = ok_songs.iter().copied().collect();
                    } else {
                        self.x_songs.clear();
                    }
                }
                if open {
                    for i in 0..self.songs.len() {
                        let s = &self.songs[i];
                        let in_pl = via_pl.contains(&i);
                        let on = in_pl || self.x_songs.contains(&i);
                        let r = tree_row(
                            ui,
                            1,
                            None,
                            Some((if on { Check::On } else { Check::Off }, !s.warn() && !in_pl)),
                            "♫",
                            &s.name,
                            false,
                            true,
                            |p, rect| paint_song_cols(p, rect, s),
                        );
                        if in_pl {
                            r.resp.on_hover_text(tr!("export.with_playlist"));
                        } else if r.check || (r.clicked && !s.warn()) {
                            if !self.x_songs.remove(&i) {
                                self.x_songs.insert(i);
                            }
                        }
                    }
                }
                // playlists
                let open = self.is_open("x:pl");
                let ok_pls: Vec<usize> = (0..self.playlists.len()).filter(|&i| !pl_warn[i]).collect();
                let c = folder_check(&self.x_playlists, ok_pls.len().max(1));
                let r = tree_row(ui, 0, Some(open), Some((c, true)), "", tr!("common.multitrack_playlists"), false, false, |p, rect| {
                    if pl_warn.iter().any(|&w| w) {
                        paint_warn(p, rect)
                    }
                });
                if r.toggle {
                    self.toggle("x:pl");
                }
                if r.check {
                    if self.x_playlists.is_empty() {
                        self.x_playlists = ok_pls.iter().copied().collect();
                    } else {
                        self.x_playlists.clear();
                    }
                }
                if open {
                    for i in 0..self.playlists.len() {
                        let on = self.x_playlists.contains(&i);
                        let len = fmt_len(self.playlist_len(&self.playlists[i].songs));
                        let warn = pl_warn[i];
                        let r = tree_row(
                            ui,
                            1,
                            None,
                            Some((if on { Check::On } else { Check::Off }, !warn)),
                            "☰",
                            &self.playlists[i].name,
                            false,
                            true,
                            |p, rect| {
                                p.text(pos2(rect.right() - 30.0, rect.center().y), Align2::RIGHT_CENTER, &len, FontId::proportional(11.5), TEXT);
                                if warn {
                                    paint_warn(p, rect)
                                }
                            },
                        );
                        if r.check || (r.clicked && !warn) {
                            if !self.x_playlists.remove(&i) {
                                self.x_playlists.insert(i);
                            }
                        }
                    }
                }
                tree_row(ui, 0, Some(false), Some((Check::Off, false)), "", tr!("export.stereo_playlists"), false, false, |_, _| {});
                tree_row(ui, 0, Some(false), Some((Check::Off, false)), "", tr!("export.stereo_songs"), false, false, |_, _| {});
            });
    }

    fn target_content(&mut self, ui: &mut Ui) {
        let p = self.preview.take().unwrap();
        if !p.target_ok {
            centered_text(ui, tr!("common.no_target"));
        } else {
            egui::ScrollArea::vertical()
                .id_source("target_content")
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    if p.music {
                        tree_row(ui, 0, Some(false), None, "🗀", sync::MUSIC, false, false, |_, _| {});
                    }
                    let open_rec = self.is_open("t:rec");
                    if tree_row(ui, 0, Some(open_rec), None, "🗀", sync::RECORDING, false, false, |_, _| {}).toggle {
                        self.toggle("t:rec");
                    }
                    if open_rec {
                        let open_pl = self.is_open("t:pl");
                        if tree_row(ui, 1, Some(open_pl), None, "🗀", sync::PLAYLISTS, false, false, |_, _| {}).toggle {
                            self.toggle("t:pl");
                        }
                        if open_pl {
                            for (n, m) in &p.plays {
                                tree_row(ui, 2, None, None, "☰", n, false, true, |pa, rect| paint_mark(pa, rect, *m));
                            }
                        }
                        for (n, m) in &p.songs {
                            tree_row(ui, 1, None, None, "♫", n, false, true, |pa, rect| paint_mark(pa, rect, *m));
                        }
                    }
                });
        }
        self.preview = Some(p);
    }

    fn export_overlay(&mut self, ctx: &egui::Context) {
        let Some(job) = self.export_job.clone() else { return };
        let j = job.lock().unwrap();
        if !j.finished {
            let (done, total, current) = (j.done, j.total, j.current.clone());
            drop(j);
            let clicked = overlay(
                ctx,
                "export_progress",
                egui::Order::Foreground,
                tr!("tab.export"),
                vec2(1460.0, 300.0),
                |ui| {
                    let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
                    ui.painter().rect_stroke(rect, 0.0, Stroke::new(1.0_f32, ORANGE));
                    let mut fill = rect.shrink(2.0);
                    fill.set_width(fill.width() * frac);
                    ui.painter().rect_filled(fill, 2.0, ORANGE);
                    ui.painter().text(
                        rect.left_center() + vec2(8.0, 0.0),
                        Align2::LEFT_CENTER,
                        tr!("export.exporting", name = current),
                        FontId::proportional(12.0),
                        Color32::WHITE,
                    );
                    ui.add_space(14.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(tr!("export.copying", n = total - done))
                            .size(15.0),
                        );
                    });
                },
                &[(tr!("common.cancel"), true)],
            );
            if clicked.is_some() {
                job.lock().unwrap().cancel = true;
            }
            return;
        }
        let ok = j.lines.iter().all(|(_, ok, _)| *ok) && !j.cancel;
        let title = if j.cancel {
            tr!("export.canceled")
        } else if ok {
            tr!("export.finished")
        } else {
            tr!("export.finished_errors")
        };
        let lines = j.lines.clone();
        let elapsed = j.elapsed;
        drop(j);
        let clicked = overlay(
            ctx,
            "export_report",
            egui::Order::Foreground,
            title,
            vec2(1460.0, 720.0),
            |ui| {
                ui.label(RichText::new(tr!("export.total_time", time = fmt_len(elapsed))).size(15.0));
                ui.add_space(6.0);
                ui.label(
                    RichText::new(tr!("export.note_speed"))
                        .italics()
                        .size(13.0),
                );
                ui.add_space(8.0);
                egui::ScrollArea::vertical().id_source("report").auto_shrink([false; 2]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    for (what, ok, msg) in &lines {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            ui.label(RichText::new(format!("- {}: ", what)).size(14.0).italics());
                            ui.label(RichText::new(msg).size(14.0).italics().color(if *ok { GREEN } else { ORANGE }));
                        });
                    }
                });
            },
            &[(tr!("common.ok"), true)],
        );
        if clicked.is_some() {
            self.export_job = None;
            self.preview_gen += 1;
            self.et_cache = None;
        }
    }

    // -----------------------------------------------------------------------
    // Import / Support
    // -----------------------------------------------------------------------

    /// scan the AudioLibrary and compare it with the project library;
    /// missing (importable) folders are checked
    fn import_rescan(&mut self) {
        let library = library_songs(&self.lib().songs_dir(), &self.config.audio_lib);
        let scan = scan_import(&self.config.import_lib, &library);
        self.import_check = match &scan {
            Ok(items) => items.iter().map(|i| i.is_new()).collect(),
            Err(_) => Vec::new(),
        };
        self.import_scan = Some(scan);
    }

    fn import_selected(&self) -> Vec<&ImportItem> {
        match &self.import_scan {
            Some(Ok(items)) => items
                .iter()
                .zip(&self.import_check)
                .filter(|(i, c)| **c && i.status == ImportStatus::Missing)
                .map(|(i, _)| i)
                .collect(),
            _ => Vec::new(),
        }
    }

    fn import_run(&mut self) {
        let dir = self.lib().songs_dir();
        let result = import_songs(&self.import_selected(), &dir, &self.config.audio_lib);
        self.refresh();
        self.import_rescan();
        match result {
            Ok(w) => {
                if !w.is_empty() {
                    self.tail_queue.push((TailCheckKind::Import, Some(w.iter().map(|p| key(p)).collect())));
                }
                let info = tr!("import.done", n = w.len());
                if !self.open_playlist_update(None, &w, &info) {
                    self.message(tr!("tab.import"), info);
                }
            }
            Err(e) => self.message(tr!("tab.import"), tr!("import.stopped", err = e)),
        }
    }

    /// Replacements for the songs of playlist `pl`: newest version of each.
    /// A version that is already in the playlist is not used again (the
    /// playlist holds two versions of that song on purpose).
    fn auto_picks(lib: &[LibrarySong], pl: &PlaylistInfo) -> Vec<Option<usize>> {
        let mut used: HashSet<String> = pl.songs.iter().map(|s| key(&s.prj)).collect();
        pl.songs
            .iter()
            .map(|s| {
                let cur = lib.iter().find(|x| key(&x.path) == key(&s.prj))?;
                let n = newest_version(cur, lib)?;
                // already in the playlist, or picked for an earlier row
                if !used.insert(key(&n.path)) {
                    return None;
                }
                lib.iter().position(|x| std::ptr::eq(x, n))
            })
            .collect()
    }

    /// "261011_ALLE-SONGS" -> "<today>_ALLE-SONGS", other names get the
    /// date in front
    fn update_name(source: &str) -> String {
        let (y, m, d, _, _) = local_time(now_ms());
        let today = format!("{:02}{:02}{:02}", y % 100, m, d);
        let b = source.as_bytes();
        if b.len() > 6 && b[..6].iter().all(|c| c.is_ascii_digit()) && b[6] == b'_' {
            format!("{}{}", today, &source[6..])
        } else {
            format!("{}_{}", today, source)
        }
    }

    /// Open "Update Playlist". `source` None = pick the playlist in which the
    /// most songs get a newer version (from `fresh` if given; on a tie the
    /// most recently changed one). Returns false if there is nothing to update.
    fn open_playlist_update(&mut self, source: Option<usize>, fresh: &[String], info: &str) -> bool {
        let lib = library_songs(&self.lib().songs_dir(), &self.config.audio_lib);
        let fresh: HashSet<String> = fresh.iter().map(|p| key(p)).collect();
        let source = match source {
            Some(i) => i,
            None => {
                let score = |pl: &PlaylistInfo| {
                    Self::auto_picks(&lib, pl)
                        .iter()
                        .flatten()
                        .filter(|&&i| fresh.is_empty() || fresh.contains(&key(&lib[i].path)))
                        .count()
                };
                let best = self
                    .playlists
                    .iter()
                    .enumerate()
                    .map(|(i, pl)| (score(pl), pl.modified, i))
                    .max();
                match best {
                    Some((n, _, i)) if n > 0 => i,
                    _ => return false,
                }
            }
        };
        let Some(pl) = self.playlists.get(source) else { return false };
        self.pl_update = Some(PlUpdate {
            source,
            name: Self::update_name(&pl.name),
            info: info.to_string(),
            picks: Self::auto_picks(&lib, pl),
            lib,
            fresh,
        });
        true
    }

    /// why the new playlist name can't be used, None = ok
    fn update_name_problem(&self, name: &str) -> Option<String> {
        let name = name.trim();
        if !valid_fat32_name(name) {
            return Some(tr!("update.name_invalid").into());
        }
        if self.playlists.iter().any(|p| p.name.eq_ignore_ascii_case(name)) {
            return Some(tr!("common.playlist_exists", name = name));
        }
        None
    }

    fn playlist_update(&mut self, ctx: &egui::Context) {
        let Some(mut up) = self.pl_update.take() else { return };
        let problem = self.update_name_problem(&up.name);
        let screen = ctx.screen_rect();
        let clicked = overlay(
            ctx,
            "pl_update",
            egui::Order::Foreground,
            tr!("update.title"),
            vec2((screen.width() - 180.0).min(1300.0), screen.height()),
            |ui| self.playlist_update_body(ui, &mut up, problem.as_deref()),
            &[(tr!("update.create"), problem.is_none()), (tr!("common.cancel"), true)],
        );
        match clicked {
            Some(0) => self.playlist_update_create(&up),
            Some(_) => {}
            None => self.pl_update = Some(up),
        }
    }

    fn playlist_update_body(&self, ui: &mut Ui, up: &mut PlUpdate, problem: Option<&str>) {
        if !up.info.is_empty() {
            ui.label(RichText::new(&up.info).size(15.0).color(ORANGE));
            ui.add_space(6.0);
        }
        egui::Grid::new("plu_head").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label(RichText::new(tr!("update.based_on")).color(TEXT));
            let before = up.source;
            egui::ComboBox::from_id_source("plu_src")
                .width(360.0)
                .selected_text(self.playlists.get(up.source).map(|p| p.name.as_str()).unwrap_or(""))
                .show_ui(ui, |ui| {
                    for (i, p) in self.playlists.iter().enumerate() {
                        ui.selectable_value(&mut up.source, i, &p.name);
                    }
                });
            if up.source != before {
                if let Some(pl) = self.playlists.get(up.source) {
                    up.picks = Self::auto_picks(&up.lib, pl);
                    up.name = Self::update_name(&pl.name);
                }
            }
            ui.end_row();
            ui.label(RichText::new(tr!("update.new_name")).color(TEXT));
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut up.name).desired_width(360.0));
                if let Some(p) = problem {
                    ui.label(RichText::new(p).color(RED));
                }
            });
            ui.end_row();
        });
        let Some(pl) = self.playlists.get(up.source) else { return };
        let changed = up.picks.iter().flatten().count();
        ui.add_space(4.0);
        ui.label(RichText::new(tr!("update.replaced", n = changed, total = pl.songs.len())).color(TEXT));
        ui.add_space(4.0);
        let lib = &up.lib;
        list_box(ui, |ui| {
            egui::ScrollArea::vertical().max_height(ui.available_height() - 8.0).auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("plu_rows").num_columns(4).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                    for h in ["#", tr!("update.current_song"), tr!("update.new_song"), ""] {
                        ui.label(RichText::new(h).color(DIM));
                    }
                    ui.end_row();
                    for (n, s) in pl.songs.iter().enumerate() {
                        let cur = lib.iter().position(|x| key(&x.path) == key(&s.prj));
                        ui.label(RichText::new(format!("{}", n + 1)).color(DIM));
                        ui.label(RichText::new(stem(&s.prj)).color(TEXT));
                        let pick = &mut up.picks[n];
                        let shown = pick.map(|i| lib[i].name.clone()).unwrap_or_else(|| tr!("update.keep").into());
                        egui::ComboBox::from_id_source(("plu_pick", n))
                            .width(380.0)
                            .selected_text(shown)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(pick, None, tr!("update.keep"));
                                // versions of the same song first, newest first
                                let ck = cur.and_then(|c| lib[c].key.clone());
                                let mut same: Vec<usize> = (0..lib.len()).filter(|&i| ck.is_some() && lib[i].key == ck && Some(i) != cur).collect();
                                same.sort_by(|&a, &b| natural_cmp(&lib[b].version, &lib[a].version));
                                for &i in &same {
                                    ui.selectable_value(pick, Some(i), &lib[i].name);
                                }
                                ui.separator();
                                for i in (0..lib.len()).filter(|i| !same.contains(i) && Some(*i) != cur) {
                                    ui.selectable_value(pick, Some(i), &lib[i].name);
                                }
                            });
                        match *pick {
                            Some(i) if up.fresh.contains(&key(&lib[i].path)) => ui.label(RichText::new(tr!("update.just_imported")).color(ORANGE)),
                            Some(_) => ui.label(RichText::new(tr!("update.newer")).color(ORANGE)),
                            None if cur.is_none() => ui.label(RichText::new(tr!("common.missing_song")).color(RED)),
                            None => ui.label(RichText::new(tr!("update.unchanged")).color(DIM)),
                        };
                        ui.end_row();
                    }
                });
            });
        });
    }

    fn playlist_update_create(&mut self, up: &PlUpdate) {
        let Some(pl) = self.playlists.get(up.source) else { return };
        let songs: Vec<PlSong> = pl
            .songs
            .iter()
            .zip(&up.picks)
            .map(|(s, p)| PlSong {
                prj: p.map(|i| up.lib[i].path.clone()).unwrap_or_else(|| s.prj.clone()),
                mode: s.mode.clone(),
                delay: s.delay.clone(),
            })
            .collect();
        let name = up.name.trim().to_string();
        if self.lib().playlist_path(&name).exists() {
            self.message(tr!("common.update_playlist"), tr!("common.playlist_exists", name = name));
            return;
        }
        let markers = pl.markers;
        let result = self.lib().create_playlist(&name, &to_pl_songs(&songs), &markers);
        if self.lib_ok(result) {
            self.refresh();
            self.select_playlist_name(&name);
            self.tab = Tab::Home;
            let n = up.picks.iter().flatten().count();
            self.message(tr!("common.update_playlist"), tr!("update.created", name = name, n = n));
        }
    }

    fn import(&mut self, ui: &mut Ui) {
        panel_tab(ui, tr!("import.title"), false);
        panel_body(ui, true, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(tr!("import.audiolib")).color(TEXT));
                let lib = if self.config.import_lib.is_empty() { tr!("import.not_set") } else { &self.config.import_lib };
                ui.label(RichText::new(lib).color(DIM));
                ui.add_space(12.0);
                if small_btn(ui, tr!("common.import_songs"), !self.config.import_lib.is_empty()) {
                    self.import_rescan();
                }
                if small_btn(ui, tr!("import.update_playlist"), !self.playlists.is_empty())
                    && !self.open_playlist_update(None, &[], "")
                {
                    self.message(tr!("common.update_playlist"), tr!("import.all_newest"));
                }
                if small_btn(ui, tr!("pack.import_btn_open"), true) {
                    let lib = self.lib();
                    if let Err(e) = self.bandpack.open(&lib) {
                        self.message(tr!("pack.import_title"), e);
                    }
                }
            });
            ui.label(
                RichText::new(tr!("import.layout"))
                    .size(11.0)
                    .color(DIM),
            );
            ui.add_space(6.0);
            let items = match &self.import_scan {
                None => {
                    centered_text(ui, tr!("import.click_scan"));
                    return;
                }
                Some(Err(e)) => {
                    centered_text(ui, e);
                    return;
                }
                Some(Ok(items)) => items.clone(),
            };
            let in_lib = |it: &ImportItem| matches!(it.status, ImportStatus::Present(_) | ImportStatus::Duplicate(_));
            let new = items.iter().filter(|i| i.is_new()).count();
            let present = items.iter().filter(|i| in_lib(i)).count();
            let old = items.iter().filter(|i| !in_lib(i) && i.older_than.is_some()).count();
            let problems = items.iter().filter(|i| matches!(i.status, ImportStatus::Invalid(_)) && i.older_than.is_none()).count();
            ui.horizontal(|ui| {
                ui.label(RichText::new(tr!("import.summary", n = items.len(), new = new, problems = problems, present = present, old = old)).color(TEXT));
                ui.add_space(16.0);
                ui.checkbox(&mut self.import_show_all, tr!("import.show_all"));
            });
            ui.add_space(4.0);
            let selected = self.import_selected().len();
            let list_h = (ui.available_height() - 40.0).max(80.0);
            list_box(ui, |ui| {
                egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
                    egui::Grid::new("import").num_columns(5).striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
                        for h in ["", tr!("import.col_song"), tr!("import.col_tracks"), "MIDI", tr!("import.col_status")] {
                            ui.label(RichText::new(h).color(DIM));
                        }
                        ui.end_row();
                        let mut shown = 0;
                        for (i, it) in items.iter().enumerate() {
                            if (in_lib(it) || it.older_than.is_some()) && !self.import_show_all {
                                continue;
                            }
                            shown += 1;
                            if it.status == ImportStatus::Missing {
                                ui.checkbox(&mut self.import_check[i], "");
                            } else {
                                ui.label("");
                            }
                            ui.label(RichText::new(&it.name).color(TEXT)).on_hover_text(&it.dir);
                            let fmt = if it.rate > 0 { tr!("import.fmt", n = it.wavs.len(), rate = fmt_sr(it.rate), bits = it.bits) } else { tr!("import.fmt_ch", n = it.wavs.len()) };
                            ui.label(RichText::new(format!("{}   {}", fmt, fmt_len(it.duration))).color(TEXT));
                            let midi = it.midi.as_deref().map(file_name).unwrap_or_else(|| "-".into());
                            ui.label(RichText::new(midi).color(TEXT));
                            let notes = if it.notes.is_empty() { String::new() } else { format!(" ({})", it.notes.join(", ")) };
                            let (text, color) = match (&it.status, &it.older_than) {
                                (ImportStatus::Missing, None) => (tr!("import.st_new", notes = notes), ORANGE),
                                (ImportStatus::Missing, Some(n)) => (tr!("import.st_old_newer", name = n, notes = notes), DIM),
                                (ImportStatus::Present(n), _) => (tr!("import.st_in_lib", name = n), GREEN),
                                (ImportStatus::Duplicate(n), _) => (tr!("import.st_in_lib", name = n.join(", ")), GREEN),
                                (ImportStatus::Invalid(e), None) => (tr!("import.st_invalid", err = e), RED),
                                (ImportStatus::Invalid(e), Some(_)) => (tr!("import.st_old_invalid", err = e), DIM),
                            };
                            ui.label(RichText::new(text).color(color));
                            ui.end_row();
                        }
                        if shown == 0 {
                            ui.label("");
                            ui.label(RichText::new(tr!("import.up_to_date")).color(GREEN));
                            ui.end_row();
                        }
                    });
                });
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if small_btn(ui, tr!("import.select_new"), new > 0) {
                    for (c, it) in self.import_check.iter_mut().zip(&items) {
                        *c = it.is_new() || (*c && it.status == ImportStatus::Missing);
                    }
                }
                if small_btn(ui, tr!("import.select_none"), selected > 0) {
                    self.import_check.iter_mut().for_each(|c| *c = false);
                }
                ui.add_space(20.0);
                if small_btn(ui, &tr!("import.import_n", n = selected), selected > 0) {
                    self.dialog = Some(Dialog::ImportConfirm);
                }
            });
        });
    }

    fn support(&mut self, ui: &mut Ui) {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            egui::Frame::none()
                .fill(PANEL)
                .stroke(Stroke::new(1.0_f32, ORANGE))
                .rounding(6.0)
                .inner_margin(Margin::same(40.0))
                .show(ui, |ui| {
                    ui.set_width(760.0);
                    ui.label(RichText::new("lp16-tool").size(64.0).color(ORANGE));
                    ui.add_space(10.0);
                    ui.label(RichText::new(tr!("support.version", v = env!("CARGO_PKG_VERSION"))).size(22.0));
                    ui.add_space(10.0);
                    ui.label(tr!("support.line1"));
                    ui.label(tr!("support.line2"));
                    ui.add_space(10.0);
                    ui.label(RichText::new(tr!("support.settings_file", path = config_path().display())).color(DIM));
                });
        });
    }

    // -----------------------------------------------------------------------
    // Edit Target
    // -----------------------------------------------------------------------

    fn edit_target(&mut self, ui: &mut Ui) {
        let h = ui.available_height();
        self.two_panels(
            ui,
            h,
            |app, ui| {
                panel_tab(ui, tr!("et.target_explorer"), false);
                panel_body(ui, true, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(tr!("et.target_location"));
                        let w = ui.available_width() - 90.0;
                        app.target_picker(ui, w);
                        if ui
                            .add(egui::Button::new(RichText::new("🔄").size(13.0)).frame(false))
                            .clicked()
                        {
                            app.drives = list_drives();
                            app.et_cache = None;
                        }
                    });
                    ui.add_space(4.0);
                    let stick_ok = app.et_stick().is_some();
                    if small_btn(ui, tr!("common.delete_unused"), stick_ok) {
                        app.et_ask_delete_unused();
                    }
                    ui.add_space(4.0);
                    app.target_tree(ui);
                });
            },
            |app, ui| {
                panel_tab(ui, tr!("common.details"), false);
                panel_body(ui, true, |ui| {
                    egui::ScrollArea::vertical()
                        .id_source("et_details")
                        .auto_shrink([false; 2])
                        .show(ui, |ui| app.target_details(ui));
                });
            },
        );
    }

    fn target_tree(&mut self, ui: &mut Ui) {
        let root = PathBuf::from(&self.config.stick_root);
        if self.config.stick_root.trim().is_empty() || !root.is_dir() {
            centered_text(ui, tr!("common.no_target"));
            return;
        }
        let stick = sync::Stick::new(&self.config.stick_root);
        let rec = stick.recording();
        let mut clicked: Option<EtSel> = None;
        let mut find: Option<String> = None;
        let mut action: Option<EtAction> = None;
        let mut rows: Vec<EtSel> = Vec::new();
        let scroll = std::mem::take(&mut self.et_scroll);
        egui::ScrollArea::vertical()
            .id_source("target_tree")
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (folder, k) in [(sync::MUSIC, "e:music"), (sync::RECORDING, "e:rec")] {
                    let base = root.join(folder);
                    if !base.is_dir() {
                        continue;
                    }
                    let open = self.is_open(k);
                    let r = tree_row(ui, 0, Some(open), None, "🗀", folder, false, false, |_, _| {});
                    if base == rec {
                        r.resp.context_menu(|ui| {
                            if ui.button(tr!("common.delete_unused")).clicked() {
                                action = Some(EtAction::DeleteUnused);
                                ui.close_menu();
                            }
                        });
                    }
                    if r.toggle {
                        self.toggle(k);
                    }
                    if !open {
                        continue;
                    }
                    let pk = format!("{}:pl", k);
                    if base.join(sync::PLAYLISTS).is_dir() {
                        let open_pl = self.is_open(&pk);
                        if tree_row(ui, 1, Some(open_pl), None, "🗀", sync::PLAYLISTS, false, false, |_, _| {}).toggle {
                            self.toggle(&pk);
                        }
                        if open_pl && base == rec {
                            for s in stick.playlists() {
                                let sel = self.et_sel == Some(EtSel::Play(s.clone()));
                                let r = tree_row(ui, 2, None, None, "☰", &s, sel, true, |_, _| {});
                                if sel && scroll {
                                    r.resp.scroll_to_me(None);
                                }
                                rows.push(EtSel::Play(s.clone()));
                                r.resp.context_menu(|ui| {
                                    if ui.button(tr!("common.delete_playlist")).clicked() {
                                        action = Some(EtAction::Delete(EtSel::Play(s.clone())));
                                        ui.close_menu();
                                    }
                                });
                                if r.clicked {
                                    clicked = Some(EtSel::Play(s));
                                }
                            }
                        }
                    }
                    if base == rec {
                        for n in stick.songs() {
                            let sel = self.et_sel == Some(EtSel::Song(n.clone()));
                            let r = tree_row(ui, 1, None, None, "♫", &n, sel, true, |_, _| {});
                            if sel && scroll {
                                r.resp.scroll_to_me(None);
                            }
                            rows.push(EtSel::Song(n.clone()));
                            r.resp.context_menu(|ui| {
                                if ui.button(tr!("common.delete_song")).clicked() {
                                    action = Some(EtAction::Delete(EtSel::Song(n.clone())));
                                    ui.close_menu();
                                }
                                if ui.button(tr!("common.delete_unused")).clicked() {
                                    action = Some(EtAction::DeleteUnused);
                                    ui.close_menu();
                                }
                            });
                            if r.double {
                                find = Some(n.clone());
                            } else if r.clicked {
                                clicked = Some(EtSel::Song(n));
                            }
                        }
                    }
                }
            })
            .inner;
        if let Some(c) = clicked {
            self.et_sel = Some(c);
        }
        self.et_rows = rows;
        if self.dialog.is_none() && !ui.ctx().wants_keyboard_input() {
            let key = |k| ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, k));
            let step = if key(egui::Key::ArrowDown) {
                Some(true)
            } else if key(egui::Key::ArrowUp) {
                Some(false)
            } else {
                None
            };
            if let Some(down) = step {
                self.et_step(down);
            }
        }
        if action.is_none() && self.dialog.is_none() && delete_key(ui) {
            action = self.et_sel.clone().map(EtAction::Delete);
        }
        match action {
            Some(EtAction::Delete(sel)) => {
                self.et_sel = Some(sel.clone());
                self.et_ask_delete(sel);
            }
            Some(EtAction::DeleteUnused) => self.et_ask_delete_unused(),
            None => {}
        }
        if let Some(n) = find {
            if let Some(i) = self.songs.iter().position(|s| s.name == n) {
                self.sel = Sel::Song(i);
                self.open.insert("h:root".into());
                self.open.insert("h:songs".into());
                self.tab = Tab::Home;
            }
        }
    }

    /// Arrow keys in the Target Explorer: select the next / previous row.
    fn et_step(&mut self, down: bool) {
        let n = self.et_rows.len();
        if n == 0 {
            return;
        }
        let cur = self.et_sel.as_ref().and_then(|s| self.et_rows.iter().position(|r| r == s));
        let i = match (cur, down) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(i), true) => (i + 1).min(n - 1),
            (Some(i), false) => i.saturating_sub(1),
        };
        self.et_sel = Some(self.et_rows[i].clone());
        self.et_scroll = true;
    }

    /// The selected stick, if its folder exists.
    fn et_stick(&self) -> Option<sync::Stick> {
        let root = self.config.stick_root.trim();
        (!root.is_empty() && Path::new(root).is_dir()).then(|| sync::Stick::new(root))
    }

    /// Ask before deleting a song or playlist from the stick.
    fn et_ask_delete(&mut self, sel: EtSel) {
        let Some(stick) = self.et_stick() else { return };
        self.dialog = Some(match sel {
            EtSel::Song(name) => Dialog::EtDeleteSong { playlists: stick.playlists_with(&name), name },
            EtSel::Play(name) => Dialog::EtDeletePlaylist(name),
        });
    }

    /// Ask before deleting every song no playlist on the stick lists.
    fn et_ask_delete_unused(&mut self) {
        let Some(stick) = self.et_stick() else { return };
        match stick.songs_not_in_playlists() {
            Ok(v) if v.is_empty() => self.message(tr!("et.delete_songs"), tr!("et.all_used")),
            Ok(v) => {
                let bytes = v.iter().map(|n| stick.song_size(n)).sum();
                self.dialog = Some(Dialog::EtDeleteUnused(v, bytes));
            }
            Err(e) => self.message(tr!("et.delete_songs"), tr!("et.playlist_unreadable", err = e)),
        }
    }

    /// Delete songs / a playlist on the stick, then refresh the views.
    fn et_delete(&mut self, songs: &[String], playlist: Option<&str>) {
        let Some(stick) = self.et_stick() else { return };
        let mut errors = Vec::new();
        let mut done = 0;
        for n in songs {
            match stick.delete_song(n) {
                Ok(()) => done += 1,
                Err(e) => errors.push(e),
            }
        }
        if let Some(p) = playlist {
            if let Err(e) = stick.delete_playlist(p) {
                errors.push(e);
            }
        }
        let gone: Vec<EtSel> = songs
            .iter()
            .map(|n| EtSel::Song(n.clone()))
            .chain(playlist.map(|p| EtSel::Play(p.to_string())))
            .collect();
        let next = self.et_sel.as_ref().and_then(|s| self.et_rows.iter().position(|r| r == s)).and_then(|i| {
            let rest = || self.et_rows.iter().filter(|r| !gone.contains(r));
            self.et_rows[i..].iter().find(|r| !gone.contains(r)).or_else(|| rest().next_back()).cloned()
        });
        self.et_sel = next;
        self.et_scroll = self.et_sel.is_some();
        self.et_cache = None;
        self.preview_gen += 1;
        if songs.len() > 1 {
            let mut text = tr!("et.deleted_n", n = done, total = songs.len());
            if !errors.is_empty() {
                text += &tr!("common.failed", list = errors.join("\n"));
            }
            self.message(tr!("et.delete_songs"), text);
        } else if !errors.is_empty() {
            self.message(tr!("common.delete"), errors.join("\n"));
        }
    }

    fn compute_et_details(&self, sel: &EtSel) -> EtDetails {
        let stick = sync::Stick::new(&self.config.stick_root);
        match sel {
            EtSel::Song(name) => {
                let info = stick.song_info(name);
                // like FAT: a playlist naming "ABCDEFU_x" uses the folder "Abcdefu_x";
                // a different spelling is shown, the next export renames the folder
                let key = name.to_lowercase();
                let related: Vec<String> = stick
                    .playlists()
                    .into_iter()
                    .filter_map(|p| {
                        let other = stick.playlist_songs(&p).into_iter().find(|n| n.to_lowercase() == key)?;
                        Some(if &other == name { p } else { tr!("et.names_it", playlist = p, name = other) })
                    })
                    .collect();
                EtDetails {
                    title: name.clone(),
                    lines: vec![
                        (tr!("common.type").into(), tr!("home.type_song").into()),
                        (tr!("common.sample_rate").into(), fmt_sr(info.sample_rate)),
                        (tr!("common.resolution").into(), tr!("common.bits", n = info.bits)),
                        (tr!("et.channel_count").into(), info.channels),
                        (tr!("common.length").into(), fmt_len(info.length)),
                        (tr!("common.size").into(), fmt_size(info.size)),
                        (tr!("common.mod_date").into(), fmt_date(info.modified)),
                        (tr!("et.midi_file").into(), info.midi_file),
                    ],
                    list_title: tr!("common.related_playlists").into(),
                    list: related,
                    play_songs: false,
                    starts: Vec::new(),
                }
            }
            EtSel::Play(name) => {
                let info = stick.playlist_info(name);
                EtDetails {
                    title: name.clone(),
                    lines: vec![
                        (tr!("common.type").into(), tr!("et.type_playlist").into()),
                        (tr!("common.song_count").into(), info.songs.len().to_string()),
                    ],
                    list_title: String::new(),
                    list: info
                        .songs
                        .iter()
                        .enumerate()
                        .map(|(i, n)| {
                            let warn = if let Some(other) = stick.song_case_differs(n) {
                                format!("   {}", tr!("et.folder_is", name = other))
                            } else if !stick.song_dir(n).is_dir() {
                                format!("   {}", tr!("et.not_on_stick"))
                            } else {
                                String::new()
                            };
                            format!("{:>3}: {}{}", i + 1, n, warn)
                        })
                        .chain(std::iter::once(String::new()))
                        .chain(
                            [
                                format!("{}:  {}", tr!("common.length"), fmt_len(info.length)),
                                format!("{}:  {}", tr!("common.size"), fmt_size(info.size)),
                                format!("{}:  {}", tr!("common.mod_date"), fmt_date(info.modified)),
                            ]
                            .into_iter(),
                        )
                        .collect(),
                    play_songs: true,
                    starts: info.starts.clone(),
                }
            }
        }
    }

    fn target_details(&mut self, ui: &mut Ui) {
        let Some(sel) = self.et_sel.clone() else { return };
        if self.et_cache.as_ref().map(|(s, _)| s != &sel).unwrap_or(true) {
            let d = self.compute_et_details(&sel);
            self.et_cache = Some((sel.clone(), d));
        }
        let d = &self.et_cache.as_ref().unwrap().1;
        ui.vertical_centered(|ui| {
            ui.add_space(4.0);
            ui.label(RichText::new(&d.title).size(18.0).strong());
        });
        ui.add_space(4.0);
        let mut delete = false;
        ui.horizontal(|ui| {
            delete = small_btn(ui, tr!("common.delete"), true);
            if d.play_songs {
                small_btn(ui, tr!("et.new_playlist"), false);
                small_btn(ui, tr!("common.edit"), false);
            } else {
                small_btn(ui, tr!("common.mixer"), false);
            }
        });
        if delete {
            self.et_ask_delete(sel);
            return;
        }
        ui.add_space(8.0);
        for (k, v) in &d.lines {
            kv(ui, k, v);
        }
        if d.play_songs {
            let n = d.list.iter().position(|l| l.is_empty()).unwrap_or(d.list.len());
            list_box(ui, |ui| {
                for (i, l) in d.list[..n].iter().enumerate() {
                    let (mode, delay) = d.starts.get(i).cloned().unwrap_or_default();
                    ui.horizontal(|ui| {
                        ui.label(l);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| mode_widget(ui, &mode, &delay));
                    });
                }
            });
            for l in d.list.iter().skip(n + 1) {
                ui.label(l);
            }
        } else {
            ui.label(&d.list_title);
            list_box(ui, |ui| {
                for l in &d.list {
                    ui.label(format!("    {}", l));
                }
            });
            ui.add_space(8.0);
            ui.label(RichText::new(tr!("et.dblclick")).size(11.5).color(DIM));
        }
    }

    // -----------------------------------------------------------------------
    // Settings
    // -----------------------------------------------------------------------

    fn settings(&mut self, ui: &mut Ui) {
        let w = ui.available_width().min(1400.0);
        let margin = (ui.available_width() - w) / 2.0;
        ui.add_space(30.0);
        ui.horizontal(|ui| {
            ui.add_space(margin);
            ui.vertical(|ui| {
                ui.set_width(w);
                self.language(ui);
                ui.add_space(12.0);
                panel_tab(ui, tr!("settings.devices"), false);
                panel_body(ui, false, |ui| {
                    ui.label(tr!("settings.devices_hint"));
                    for (name, on) in [("uTrack-X32", false), ("uTrack24", false), ("LP-16", true), ("LR-16", false)] {
                        let mut v = on;
                        ui.add_enabled(false, egui::Checkbox::new(&mut v, name));
                    }
                });
                ui.add_space(12.0);
                panel_tab(ui, tr!("settings.file_location"), false);
                let mut save = false;
                let mut init = false;
                panel_body(ui, false, |ui| {
                    egui::Grid::new("paths").num_columns(3).spacing([8.0, 6.0]).show(ui, |ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(tr!("settings.library_folder")));
                        if folder_btn(ui, tr!("settings.choose_folder")) {
                            let mut dlg = rfd::FileDialog::new().set_title(tr!("settings.library_folder_title"));
                            if Path::new(&self.config.library).is_dir() {
                                dlg = dlg.set_directory(&self.config.library);
                            }
                            if let Some(dir) = dlg.pick_folder() {
                                self.config.library = dir.to_string_lossy().into_owned();
                                save = true;
                            }
                        }
                        let r = ui.add(egui::TextEdit::singleline(&mut self.config.library).desired_width(w - 260.0));
                        if r.changed() {
                            self.settings_dirty = true;
                        }
                        if r.lost_focus() && self.settings_dirty {
                            save = true;
                        }
                        ui.end_row();
                        // derived folders (read only), with a hint if missing
                        let mut derived = self.config.clone();
                        derived.apply_library();
                        for (label, path) in [(tr!("settings.project_library"), derived.project_lib), (tr!("settings.audio_library"), derived.audio_lib)] {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(RichText::new(label).color(DIM)));
                            ui.label("");
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(if path.is_empty() { "-" } else { &path }).color(DIM));
                                if !path.is_empty() && !Path::new(&path).is_dir() {
                                    ui.label(RichText::new(tr!("settings.missing")).color(ORANGE));
                                }
                            });
                            ui.end_row();
                        }
                    });
                    ui.label(
                        RichText::new(tr!("settings.library_hint", prj = lp16_tool::PROJECT_LIB_NAME, audio = lp16_tool::AUDIO_LIB_NAME))
                        .size(11.0)
                        .color(DIM),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if small_btn(ui, tr!("settings.init_folders"), !self.config.library.trim().is_empty()) {
                            init = true;
                        }
                        ui.label(
                            RichText::new(tr!("settings.init_hint"))
                                .size(11.0)
                                .color(DIM),
                        );
                    });
                });
                if save {
                    self.save_settings();
                }
                if init {
                    self.init_folders();
                }
                ui.add_space(12.0);
                self.audio_output(ui);
                ui.add_space(12.0);
                self.midi_output(ui);
            });
        });
    }

    /// The language area of the Settings tab: a change applies at once and
    /// is saved in lp16-tool.toml ("language").
    fn language(&mut self, ui: &mut Ui) {
        panel_tab(ui, "Language / Sprache", false);
        let mut lang = i18n::lang();
        panel_body(ui, false, |ui| {
            ui.horizontal(|ui| {
                ui.label(tr!("settings.language"));
                egui::ComboBox::from_id_source("language")
                    .selected_text(lang.native_name())
                    .show_ui(ui, |ui| {
                        for l in Lang::ALL {
                            ui.selectable_value(&mut lang, l, l.native_name());
                        }
                    });
            });
        });
        if lang != i18n::lang() {
            i18n::set_lang(lang);
            self.config.language = lang.code().to_string();
            if let Err(e) = save_config(&config_path(), &self.config) {
                self.message(tr!("common.settings"), tr!("common.cannot_save", path = config_path().display(), err = e));
            }
            ui.ctx().request_repaint();
        }
    }

    /// The "Audio output" area of the Settings tab (gui_audio.rs): changes
    /// are saved at once and handed to the rehearsal player, which uses
    /// them the next time it opens its stream.
    fn audio_output(&mut self, ui: &mut Ui) {
        let running = self.rehearsal.session.stream_info().cloned();
        let playing = self.rehearsal.session.is_playing();
        let error = self.rehearsal.session.error().map(str::to_string);
        let out = self.audio.ui(ui, &mut self.config, running.as_ref(), playing, error.as_deref());
        if out.changed {
            self.rehearsal.session.set_settings(AudioSettings::from(&self.config));
            if let Err(e) = save_config(&config_path(), &self.config) {
                self.message(tr!("common.settings"), tr!("common.cannot_save", path = config_path().display(), err = e));
            }
        }
        if out.tone {
            // the tone may need the device that the idle stream holds
            self.rehearsal.session.park();
            self.audio.start_tone(&self.config);
        }
    }

    /// The "MIDI output" area of the Settings tab (gui_midi.rs): the port
    /// is saved at once and handed to the rehearsal player, which opens it
    /// at the next Play.
    fn midi_output(&mut self, ui: &mut Ui) {
        let status = self.rehearsal.session.midi_status();
        let can_test = self.rehearsal.session.has_midi_file();
        let out = self.midi.ui(ui, &mut self.config, can_test, &status);
        if out.changed {
            self.rehearsal.session.set_midi_port(&self.config.midi_port);
            if let Err(e) = save_config(&config_path(), &self.config) {
                self.message(tr!("common.settings"), tr!("common.cannot_save", path = config_path().display(), err = e));
            }
        }
        if out.test {
            let sent = self.rehearsal.session.send_midi_test();
            self.midi.set_test_note(if sent { None } else { Some(tr!("settings.midi_unreadable").to_string()) });
        }
    }

    // -----------------------------------------------------------------------
    // overlays: playlist editor, song viewer, dialogs
    // -----------------------------------------------------------------------

    fn playlist_editor(&mut self, ctx: &egui::Context) {
        let Some(mut ed) = self.pl_editor.take() else { return };
        let can_save = ed.dirty;
        let screen = ctx.screen_rect();
        let clicked = overlay(
            ctx,
            "pl_editor",
            egui::Order::Foreground,
            tr!("common.edit_playlist"),
            vec2(screen.width() - 180.0, screen.height()),
            |ui| self.playlist_editor_body(ui, &mut ed),
            &[(tr!("common.save"), can_save), (tr!("common.close_editor"), true)],
        );
        match ed.drag {
            Some(PlDrag::Rows) => {
                let sel: Vec<usize> = (0..ed.items.len()).filter(|&k| ed.checked[k]).collect();
                let text = if sel.len() == 1 { item_label(&ed.items[sel[0]]) } else { tr!("pled.rows", n = sel.len()) };
                paint_drag_ghost(ctx, text);
            }
            Some(PlDrag::Song(i)) => {
                if let Some(s) = self.songs.get(i) {
                    paint_drag_ghost(ctx, s.name.clone());
                }
            }
            None => {}
        }
        // the pause is a global setting: saved once the value stops changing
        if ed.pause != self.config.set_pause && !ctx.input(|i| i.pointer.any_down()) {
            self.config.set_pause = ed.pause;
            if let Err(e) = save_set_pause(&config_path(), ed.pause) {
                self.message(tr!("common.settings"), tr!("common.cannot_save", path = config_path().display(), err = e));
            }
        }
        self.pl_editor = Some(ed);
        match clicked {
            Some(0) => self.editor_save(None),
            Some(1) => {
                if self.pl_editor.as_ref().map(|e| e.dirty).unwrap_or(false) {
                    self.dialog = Some(Dialog::EditorClose);
                } else {
                    self.pl_editor = None;
                }
            }
            _ => {}
        }
    }

    fn playlist_editor_body(&self, ui: &mut Ui, ed: &mut PlaylistEditor) {
        let (songs, markers) = split_items(&ed.items);
        let tl = self.concert_timeline(&songs, &markers, ed.pause);
        // Playlist Settings
        panel_tab(ui, tr!("pled.settings"), false);
        panel_body(ui, false, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("     {}", tr!("pled.location")));
                ui.label("LP16\\Multitrack Playlists");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(140.0);
                    ui.label(tr!("pled.song_count", n = songs.len()));
                });
            });
            ui.horizontal(|ui| {
                ui.label(tr!("pled.name"));
                let w = ui.available_width() - 360.0;
                if ui.add(egui::TextEdit::singleline(&mut ed.name).desired_width(w)).changed() {
                    ed.dirty = true;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(100.0);
                    ui.label(tr!("pled.length", time = fmt_len(self.playlist_len(&songs))));
                });
            });
            ui.horizontal(|ui| {
                ui.label(tr!("pled.pause"));
                ui.add(egui::DragValue::new(&mut ed.pause).clamp_range(0..=MAX_SET_PAUSE).suffix(" s"))
                    .on_hover_text(tr!("pled.pause_tip"));
                ui.label(RichText::new(tr!("pled.all_playlists")).color(DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(100.0);
                    ui.label(RichText::new(tr!("pled.concert", time = fmt_clock(tl.concert))).color(ORANGE));
                    if let Some(main) = tl.main_set {
                        ui.label(tr!("pled.main_set", time = fmt_clock(main), n = tl.encore_count));
                    }
                });
            });
        });
        ui.add_space(12.0);
        let h = ui.available_height();
        let w = ui.available_width();
        let half = (w - 30.0) / 2.0;
        let top = ui.cursor().min;
        let mut add: Option<usize> = None;
        // Available Songs
        ui.allocate_ui_at_rect(Rect::from_min_size(top, vec2(half, h)), |ui| {
            ui.horizontal(|ui| {
                panel_tab(ui, tr!("pled.available"), false);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if small_btn(ui, tr!("pled.add"), ed.avail_sel.is_some()) {
                        add = ed.avail_sel;
                    }
                });
            });
            panel_body(ui, true, |ui| {
                egui::ScrollArea::vertical().id_source("avail").auto_shrink([false; 2]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let r = tree_row(ui, 0, Some(ed.avail_open), None, "", tr!("common.multitrack_songs"), false, false, |_, _| {});
                    if r.toggle {
                        ed.avail_open = !ed.avail_open;
                    }
                    if ed.avail_open {
                        for (i, s) in self.songs.iter().enumerate() {
                            let r = tree_row(ui, 1, None, None, "♫", &s.name, ed.avail_sel == Some(i), true, |p, rect| paint_song_cols(p, rect, s));
                            if r.drag_started {
                                ed.avail_sel = Some(i);
                                ed.drag = Some(PlDrag::Song(i));
                            } else if r.double {
                                add = Some(i);
                            } else if r.clicked {
                                ed.avail_sel = Some(i);
                            }
                        }
                    }
                });
            });
        });
        // Playlist Content
        let mut delete = false;
        let mut new_marker: Option<PlItem> = None;
        // gap (0..=len) under the mouse while something is dragged over the content
        let mut gap: Option<usize> = None;
        let mut move_by: i32 = 0;
        let mut apply_mode = false;
        ui.allocate_ui_at_rect(Rect::from_min_size(top + vec2(half + 30.0, 0.0), vec2(half, h)), |ui| {
            panel_tab(ui, tr!("pled.content"), false);
            panel_body(ui, true, |ui| {
                let any = ed.checked.iter().any(|&c| c);
                ui.horizontal(|ui| {
                    if small_btn(ui, tr!("pled.add_encores"), markers.encore.is_none()) {
                        new_marker = Some(PlItem::Encore);
                    }
                    if small_btn(ui, tr!("pled.add_end"), markers.end.is_none()) {
                        new_marker = Some(PlItem::End);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        delete = small_btn(ui, tr!("common.delete"), any);
                        if small_btn(ui, tr!("pled.down"), any) {
                            move_by = 1;
                        }
                        if small_btn(ui, tr!("pled.up"), any) {
                            move_by = -1;
                        }
                    });
                });
                // start mode of the checked songs (uTool2: "Apply to selected rows")
                let any_song = ed.items.iter().zip(&ed.checked).any(|(it, &c)| c && matches!(it, PlItem::Song(_)));
                ui.horizontal(|ui| {
                    ui.label(RichText::new(tr!("pled.start_mode_label")).size(11.5));
                    // toggle buttons, not a ComboBox: its popup stays hidden
                    // behind this overlay
                    for m in StartMode::ALL {
                        ui.selectable_value(&mut ed.start_mode, m, RichText::new(m.label()).size(11.5)).on_hover_text(start_mode_help(m));
                    }
                    if ed.start_mode == StartMode::Timer {
                        ui.add(egui::DragValue::new(&mut ed.start_delay).clamp_range(0..=MAX_START_DELAY).suffix(" s"))
                            .on_hover_text(tr!("pled.delay_tip"));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let r = ui.add_enabled(
                            any_song,
                            egui::Button::new(RichText::new(tr!("pled.apply")).size(11.5).color(TEXT))
                                .fill(Color32::from_rgb(0x3a, 0x3a, 0x3a))
                                .stroke(Stroke::new(1.0_f32, ORANGE))
                                .rounding(3.0),
                        );
                        apply_mode = r.clicked();
                        r.on_disabled_hover_text(tr!("pled.check_first"));
                    });
                });
                // header
                let (hr, hresp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                let p = ui.painter_at(hr);
                let all = !ed.checked.is_empty() && ed.checked.iter().all(|&c| c);
                let state = if all { Check::On } else if any { Check::Partial } else { Check::Off };
                paint_check(&p, Rect::from_center_size(pos2(hr.left() + 12.0, hr.center().y), vec2(14.0, 14.0)), state, true);
                let f = FontId::proportional(11.0);
                p.text(pos2(hr.left() + 70.0, hr.center().y), Align2::CENTER_CENTER, tr!("pled.col_start_mode"), f.clone(), TEXT);
                p.text(pos2((hr.left() + hr.right() - 350.0) / 2.0, hr.center().y), Align2::CENTER_CENTER, tr!("pled.col_song"), f.clone(), TEXT);
                p.text(pos2(hr.right() - COL_START, hr.center().y), Align2::CENTER_CENTER, tr!("pled.col_start"), f.clone(), TEXT);
                p.text(pos2(hr.right() - COL_END, hr.center().y), Align2::CENTER_CENTER, tr!("pled.col_end"), f.clone(), TEXT);
                p.text(pos2(hr.right() - 120.0, hr.center().y), Align2::CENTER_CENTER, tr!("pled.col_props"), f.clone(), TEXT);
                p.text(pos2(hr.right() - 40.0, hr.center().y), Align2::CENTER_CENTER, tr!("common.length"), f, TEXT);
                if hresp.clicked() && hresp.interact_pointer_pos().map(|q| q.x < hr.left() + 24.0).unwrap_or(false) {
                    let v = !all;
                    ed.checked.iter_mut().for_each(|c| *c = v);
                }
                if any && delete_key(ui) {
                    delete = true;
                }
                let modifiers = ui.input(|i| i.modifiers);
                egui::ScrollArea::vertical().id_source("content").auto_shrink([false; 2]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let view = ui.clip_rect();
                    let pointer = if ed.drag.is_some() { ui.input(|i| i.pointer.hover_pos()) } else { None };
                    let over = pointer.filter(|q| view.contains(*q));
                    // scroll while dragging near the upper/lower edge
                    if let Some(q) = over {
                        let edge = 28.0;
                        let dy = if q.y < view.top() + edge {
                            (view.top() + edge - q.y) * 0.5
                        } else if q.y > view.bottom() - edge {
                            -(q.y - (view.bottom() - edge)) * 0.5
                        } else {
                            0.0
                        };
                        if dy != 0.0 {
                            ui.scroll_with_delta(vec2(0.0, dy));
                            ui.ctx().request_repaint();
                        }
                    }
                    let mut gap_y: Option<f32> = None;
                    // number of songs before the current row
                    let mut n = 0usize;
                    for k in 0..ed.items.len() {
                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click_and_drag());
                        if let Some(q) = over {
                            if gap.is_none() && q.y < rect.center().y {
                                gap = Some(k);
                                gap_y = Some(rect.top());
                            }
                        }
                        let p = ui.painter_at(rect);
                        let cy = rect.center().y;
                        match &ed.items[k] {
                            PlItem::Song(s) => {
                                if ed.checked[k] {
                                    p.rect_filled(rect, 0.0, SEL);
                                } else if resp.hovered() {
                                    p.rect_filled(rect, 0.0, HOVER);
                                }
                                let times = tl.rows.get(n).copied().flatten();
                                // after the end of the concert: for information only
                                let color = if times.is_some() { TEXT } else { DIM };
                                n += 1;
                                paint_check(&p, Rect::from_center_size(pos2(rect.left() + 12.0, cy), vec2(14.0, 14.0)), if ed.checked[k] { Check::On } else { Check::Off }, true);
                                p.text(pos2(rect.left() + 22.0, cy), Align2::LEFT_CENTER, n.to_string(), FontId::proportional(13.0), color);
                                paint_mode(&p, pos2(rect.left() + 70.0, cy), &s.mode, &s.delay);
                                let mode_cell = Rect::from_center_size(pos2(rect.left() + 70.0, cy), vec2(44.0, ROW_H));
                                if ed.drag.is_none() && ui.rect_contains_pointer(mode_cell) {
                                    egui::show_tooltip_text(ui.ctx(), egui::Id::new(("pl_mode_tip", k)), start_mode_text(&s.mode, &s.delay));
                                }
                                let name_clip = Rect::from_min_max(rect.min, pos2(rect.right() - COL_START - 32.0, rect.bottom()));
                                p.with_clip_rect(name_clip).text(pos2(rect.left() + 102.0, cy), Align2::LEFT_CENTER, stem(&s.prj), FontId::proportional(13.5), color);
                                if let Some((start, end)) = times {
                                    p.text(pos2(rect.right() - COL_START, cy), Align2::CENTER_CENTER, fmt_clock(start), FontId::proportional(11.5), TEXT);
                                    p.text(pos2(rect.right() - COL_END, cy), Align2::CENTER_CENTER, fmt_clock(end), FontId::proportional(11.5), TEXT);
                                }
                                match self.song_of(&s.prj) {
                                    Some(info) => paint_song_cols(&p, rect, info),
                                    None => {
                                        p.text(pos2(rect.right() - 120.0, cy), Align2::CENTER_CENTER, tr!("pled.missing"), FontId::proportional(11.0), ORANGE);
                                        paint_warn(&p, rect);
                                    }
                                }
                            }
                            item => {
                                // a marker row, tinted like the ZUGABEN row of the old sheet
                                p.rect_filled(rect, 0.0, if ed.checked[k] { MARKER_SEL } else { MARKER_FILL });
                                if resp.hovered() && !ed.checked[k] {
                                    p.rect_stroke(rect.shrink(0.5), 0.0, Stroke::new(1.0_f32, ORANGE));
                                }
                                paint_check(&p, Rect::from_center_size(pos2(rect.left() + 12.0, cy), vec2(14.0, 14.0)), if ed.checked[k] { Check::On } else { Check::Off }, true);
                                let (text, time) = match item {
                                    PlItem::Encore => {
                                        let t = if tl.main_set.is_some() { tr!("common.encores_n", n = tl.encore_count) } else { tr!("common.encores_after").into() };
                                        (t, tl.main_set)
                                    }
                                    _ => (tr!("common.end_of_concert").to_string(), Some(tl.concert)),
                                };
                                p.text(pos2(rect.left() + 102.0, cy), Align2::LEFT_CENTER, text, FontId::proportional(13.0), ORANGE);
                                if let Some(t) = time {
                                    p.text(pos2(rect.right() - COL_END, cy), Align2::CENTER_CENTER, fmt_clock(t), FontId::proportional(11.5), ORANGE);
                                }
                            }
                        }
                        if resp.drag_started() {
                            if !ed.checked[k] {
                                list_click(&mut ed.checked, &mut ed.anchor, k, egui::Modifiers::NONE, false, |_| true);
                            }
                            ed.drag = Some(PlDrag::Rows);
                        } else if resp.clicked() {
                            let cb = Rect::from_center_size(pos2(rect.left() + 12.0, cy), vec2(14.0, 14.0));
                            let in_cb = resp.interact_pointer_pos().map(|q| cb.expand(3.0).contains(q)).unwrap_or(false);
                            list_click(&mut ed.checked, &mut ed.anchor, k, modifiers, in_cb, |_| true);
                        }
                    }
                    if over.is_some() && gap.is_none() {
                        gap = Some(ed.items.len());
                        gap_y = Some(ui.cursor().top());
                    }
                    // insertion mark
                    if let Some(y) = gap_y {
                        let x = ui.max_rect().x_range();
                        let p = ui.painter();
                        p.hline(x, y, Stroke::new(2.0_f32, ORANGE));
                        p.circle_filled(pos2(x.min + 3.0, y), 3.5, ORANGE);
                    }
                });
            });
        });
        ui.allocate_rect(Rect::from_min_size(top, vec2(w, h)), Sense::hover());
        if ed.drag.is_some() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            ed.drag = None;
        }
        if ed.drag.is_some() && ui.input(|i| i.pointer.any_released()) {
            match (ed.drag.take(), gap) {
                (Some(PlDrag::Rows), Some(g)) => {
                    let before = ed.items.clone();
                    ed.checked = move_checked(&mut ed.items, &ed.checked, g);
                    ed.anchor = ed.checked.iter().position(|&c| c);
                    if ed.items != before {
                        ed.dirty = true;
                    }
                }
                (Some(PlDrag::Song(i)), Some(g)) => {
                    ed.items.insert(g, PlItem::Song(new_pl_song(&self.songs[i].path)));
                    ed.checked = (0..ed.items.len()).map(|k| k == g).collect();
                    ed.anchor = Some(g);
                    ed.dirty = true;
                }
                _ => {}
            }
        }
        if apply_mode {
            let mode = ed.start_mode.as_str();
            let delay = if ed.start_mode == StartMode::Timer { ed.start_delay.to_string() } else { "0".to_string() };
            for (it, &c) in ed.items.iter_mut().zip(&ed.checked) {
                if let (PlItem::Song(s), true) = (it, c) {
                    if s.mode != mode || s.delay != delay {
                        s.mode = mode.to_string();
                        s.delay = delay.clone();
                        ed.dirty = true;
                    }
                }
            }
        }
        if let Some(i) = add {
            ed.items.push(PlItem::Song(new_pl_song(&self.songs[i].path)));
            ed.checked.push(false);
            ed.dirty = true;
        }
        if let Some(m) = new_marker {
            // after the last checked row, else at the end
            let at = ed.checked.iter().rposition(|&c| c).map(|k| k + 1).unwrap_or(ed.items.len());
            ed.items.insert(at, m);
            ed.checked = (0..ed.items.len()).map(|k| k == at).collect();
            ed.anchor = Some(at);
            ed.dirty = true;
        }
        if delete {
            let mut k = 0;
            ed.items.retain(|_| {
                let keep = !ed.checked[k];
                k += 1;
                keep
            });
            ed.checked = vec![false; ed.items.len()];
            ed.anchor = None;
            ed.dirty = true;
        }
        if move_by != 0 {
            let n = ed.items.len();
            let order: Vec<usize> = if move_by < 0 { (1..n).collect() } else { (0..n.saturating_sub(1)).rev().collect() };
            for i in order {
                let j = (i as i32 + move_by) as usize;
                if ed.checked[i] && !ed.checked[j] {
                    ed.items.swap(i, j);
                    ed.checked.swap(i, j);
                }
            }
            ed.dirty = true;
        }
    }

    /// Load a song of the library into the rehearsal player, switch to its
    /// tab and start playing.
    fn play_in_rehearsal(&mut self, index: usize) {
        let path = self.songs[index].path.clone();
        match SongProject::load(&path, &self.config.audio_lib) {
            Ok(prj) => {
                self.rehearsal.session.play_song(&prj);
                self.tab = Tab::Rehearsal;
            }
            Err(e) => self.message(tr!("common.play_in_rehearsal"), tr!("common.song_cant_open", err = e)),
        }
    }

    /// Home > song > Mixer: load the song into the rehearsal player without
    /// starting it and show its song mix in the mixer of the Rehearsal tab.
    fn open_in_mixer(&mut self, index: usize) {
        let path = self.songs[index].path.clone();
        self.load_in_mixer(&path, tr!("common.mixer"));
    }

    /// Load the song file `path` (stopped), switch to the Rehearsal tab and
    /// edit the song mix there.
    fn load_in_mixer(&mut self, path: &str, title: &str) {
        match SongProject::load(path, &self.config.audio_lib) {
            Ok(prj) => {
                self.rehearsal.session.load_song(&prj);
                self.rehearsal.session.edit_song_mix();
                self.tab = Tab::Rehearsal;
            }
            Err(e) => self.message(title, tr!("common.song_cant_open", err = e)),
        }
    }

    /// Home > playlist > Play in Rehearsal: load the playlist, wait on its
    /// first song, show the tab.
    fn play_playlist_in_rehearsal(&mut self, index: usize) {
        let name = self.playlists[index].name.clone();
        self.rehearsal.session.play_playlist(&name);
        self.tab = Tab::Rehearsal;
    }

    fn open_song_editor(&mut self, i: usize) {
        if let Some(s) = self.songs.get(i) {
            match SongEditor::load(&s.path, &self.config.audio_lib) {
                Ok(ed) => self.song_ed = Some(ed),
                Err(e) => self.message(tr!("common.edit_song"), tr!("common.song_cant_open", err = e)),
            }
        }
    }

    /// "Edit Multitrack Song": audio file browser on the left, 16 track
    /// slots and the MIDI file on the right. WAVs are dragged onto the slots
    /// (several at once: checked files fill consecutive slots).
    fn song_editor(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        let Some(mut ed) = self.song_ed.take() else { return };
        let audio_lib = self.config.audio_lib.clone();
        // files dropped from the Windows Explorer
        let dropped: Vec<String> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.as_ref().map(|p| p.to_string_lossy().into_owned()))
                .collect()
        });
        if !dropped.is_empty() && self.dialog.is_none() {
            ed.os_drop(ctx, &dropped, &audio_lib);
        }
        let screen = ctx.screen_rect();
        let can_save = ed.dirty;
        let clicked = overlay(
            ctx,
            "song_editor",
            egui::Order::Foreground,
            tr!("common.edit_song"),
            vec2(screen.width() - 180.0, screen.height()),
            |ui| song_editor_body(ui, &mut ed, &audio_lib, frame),
            &[(tr!("common.save"), can_save), (tr!("common.close_editor"), true)],
        );
        // drag ghost next to the mouse pointer
        if let Some(files) = &ed.drag {
            let text = if files.len() == 1 {
                Path::new(&files[0]).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            } else {
                tr!("songed.files", n = files.len())
            };
            paint_drag_ghost(ctx, text);
        }
        if let Some(text) = ed.message.take() {
            self.message(tr!("common.edit_song"), text);
        }
        let path = ed.path.clone();
        let open_mixer = std::mem::take(&mut ed.open_mixer);
        self.song_ed = Some(ed);
        if open_mixer {
            self.song_ed = None;
            self.load_in_mixer(&path, tr!("common.edit_song"));
            return;
        }
        match clicked {
            Some(0) => {
                self.rehearsal.session.song_changed(&path);
                let lib = self.lib();
                let ed = self.song_ed.as_mut().unwrap();
                match ed.save(&lib) {
                    Ok(()) => {
                        self.refresh();
                        self.select_song_path(Path::new(&path));
                    }
                    Err(e) => self.message(tr!("common.edit_song"), tr!("songed.save_failed", err = e)),
                }
            }
            Some(1) => {
                if self.song_ed.as_ref().map(|e| e.dirty).unwrap_or(false) {
                    self.dialog = Some(Dialog::SongEditorClose);
                } else {
                    self.song_ed = None;
                }
            }
            _ => {}
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.dialog.take() else { return };
        let order = egui::Order::Foreground;
        let size = vec2(920.0, 300.0);
        let mut close = false;
        if matches!(d, Dialog::NewSong { .. }) {
            close = self.new_song_dialog_ui(ctx, &mut d);
            if !close && self.dialog.is_none() {
                self.dialog = Some(d);
            }
            return;
        }
        if matches!(d, Dialog::SongName { .. } | Dialog::PlaylistName { .. }) {
            close = self.name_dialog(ctx, &mut d);
            if !close && self.dialog.is_none() {
                self.dialog = Some(d);
            }
            return;
        }
        match &mut d {
            Dialog::Message { title, text } => {
                if overlay(ctx, "dlg", order, title, size, |ui| centered_text(ui, text), &[(tr!("common.ok"), true)]).is_some() {
                    close = true;
                }
            }
            Dialog::SongName { .. } | Dialog::PlaylistName { .. } | Dialog::NewSong { .. } => {}
            Dialog::FixTail { song, tracks, trim_ms } => {
                let fade_label = tr!("tail.fade_btn", ms = format!("{:.0}", wav_tail::FADE_MS));
                let can_cut = *trim_ms >= 1.0;
                let clicked = overlay(
                    ctx,
                    "dlg",
                    order,
                    tr!("tail.no_clean"),
                    vec2(920.0, 400.0),
                    |ui| {
                        ui.label(tr!("tail.not_clean"));
                        ui.add_space(6.0);
                        egui::ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
                            for (name, _, r) in tracks.iter() {
                                ui.label(format!("    {}:  {}", name, r.describe()));
                            }
                        });
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.label(tr!("tail.cut_label"));
                            ui.add(egui::DragValue::new(trim_ms).clamp_range(0.0..=2000.0).speed(1.0).suffix(" ms"));
                            ui.label(tr!("tail.suggested"));
                        });
                        ui.add_space(10.0);
                        ui.label(tr!("tail.explain", fade = fade_label, cut = tr!("tail.cut_btn")));
                        ui.label(tr!("tail.originals_note"));
                    },
                    &[(fade_label.as_str(), true), (tr!("tail.cut_btn"), can_cut), (tr!("common.cancel"), true)],
                );
                if let Some(b) = clicked {
                    close = true;
                    if b < 2 {
                        let wavs: Vec<String> = tracks.iter().map(|(_, w, _)| w.clone()).collect();
                        let trim = if b == 1 { trim_ms.round() } else { 0.0 };
                        self.fix_tail_run(*song, &wavs, trim);
                    }
                }
            }
            Dialog::ExportTails(text) => {
                match overlay(
                    ctx,
                    "dlg",
                    order,
                    tr!("tab.export"),
                    vec2(920.0, 420.0),
                    |ui| centered_text(ui, text),
                    &[(tr!("dlg.export_anyway"), true), (tr!("common.no_cancel"), true)],
                ) {
                    Some(0) => {
                        close = true;
                        self.start_export(ctx);
                    }
                    Some(_) => close = true,
                    None => {}
                }
            }
            Dialog::ImportConfirm => {
                let names: Vec<String> = self.import_selected().iter().map(|i| i.name.clone()).collect();
                let mut list = names.iter().take(12).cloned().collect::<Vec<_>>().join("\n");
                if names.len() > 12 {
                    list += &tr!("common.and_more", n = names.len() - 12);
                }
                match overlay(
                    ctx,
                    "dlg",
                    order,
                    tr!("common.import_songs"),
                    vec2(920.0, 420.0),
                    |ui| centered_text(ui, &tr!("dlg.import_q", n = names.len(), list = list)),
                    &[(tr!("dlg.yes_import"), true), (tr!("common.no_cancel"), true)],
                ) {
                    Some(0) => {
                        close = true;
                        self.import_run();
                    }
                    Some(_) => close = true,
                    None => {}
                }
            }
            Dialog::DeleteSong(i) => {
                let name = self.songs.get(*i).map(|s| s.name.clone()).unwrap_or_default();
                let i = *i;
                match overlay(
                    ctx,
                    "dlg",
                    order,
                    tr!("common.delete_song"),
                    size,
                    |ui| centered_text(ui, &tr!("dlg.delete_song_q", name = name)),
                    &[(tr!("common.yes_delete_song"), true), (tr!("common.no_cancel"), true)],
                ) {
                    Some(0) => {
                        close = true;
                        self.delete_song(i);
                    }
                    Some(_) => close = true,
                    None => {}
                }
            }
            Dialog::DeletePlaylist(i) => {
                let name = self.playlists.get(*i).map(|s| s.name.clone()).unwrap_or_default();
                let i = *i;
                match overlay(
                    ctx,
                    "dlg",
                    order,
                    tr!("common.delete_playlist"),
                    size,
                    |ui| centered_text(ui, &tr!("dlg.delete_playlist_q", name = name)),
                    &[(tr!("common.yes_delete_playlist"), true), (tr!("common.no_cancel"), true)],
                ) {
                    Some(0) => {
                        close = true;
                        self.delete_playlist(i);
                    }
                    Some(_) => close = true,
                    None => {}
                }
            }
            Dialog::EtDeleteSong { name, playlists } => {
                let mut text = tr!("dlg.et_delete_song_q", name = name);
                if !playlists.is_empty() {
                    text += &tr!("dlg.et_still_listed", list = playlists.join("\n"));
                }
                match overlay(
                    ctx,
                    "dlg",
                    order,
                    tr!("dlg.et_delete_song"),
                    vec2(920.0, 360.0),
                    |ui| centered_text(ui, &text),
                    &[(tr!("common.yes_delete_song"), true), (tr!("common.no_cancel"), true)],
                ) {
                    Some(0) => {
                        close = true;
                        let name = name.clone();
                        self.et_delete(&[name], None);
                    }
                    Some(_) => close = true,
                    None => {}
                }
            }
            Dialog::EtDeletePlaylist(name) => {
                match overlay(
                    ctx,
                    "dlg",
                    order,
                    tr!("dlg.et_delete_playlist"),
                    size,
                    |ui| centered_text(ui, &tr!("dlg.et_delete_playlist_q", name = name)),
                    &[(tr!("common.yes_delete_playlist"), true), (tr!("common.no_cancel"), true)],
                ) {
                    Some(0) => {
                        close = true;
                        let name = name.clone();
                        self.et_delete(&[], Some(&name));
                    }
                    Some(_) => close = true,
                    None => {}
                }
            }
            Dialog::EtDeleteUnused(names, bytes) => {
                let mut list = names.iter().take(12).cloned().collect::<Vec<_>>().join("\n");
                if names.len() > 12 {
                    list += &tr!("common.and_more", n = names.len() - 12);
                }
                let text = tr!("dlg.et_delete_unused_q", n = names.len(), size = fmt_size(*bytes), list = list);
                match overlay(
                    ctx,
                    "dlg",
                    order,
                    tr!("dlg.et_delete_unused"),
                    vec2(920.0, 460.0),
                    |ui| centered_text(ui, &text),
                    &[(tr!("dlg.yes_delete_songs"), true), (tr!("common.no_cancel"), true)],
                ) {
                    Some(0) => {
                        close = true;
                        let names = names.clone();
                        self.et_delete(&names, None);
                    }
                    Some(_) => close = true,
                    None => {}
                }
            }
            Dialog::EditorSaveAs => {
                match overlay(
                    ctx,
                    "dlg",
                    order,
                    tr!("dlg.playlist_rename"),
                    size,
                    |ui| centered_text(ui, tr!("dlg.rename_playlist_q")),
                    &[(tr!("common.rename"), true), (tr!("common.copy"), true), (tr!("common.cancel"), true)],
                ) {
                    Some(0) => {
                        close = true;
                        self.editor_save(Some(false));
                    }
                    Some(1) => {
                        close = true;
                        self.editor_save(Some(true));
                    }
                    Some(_) => close = true,
                    None => {}
                }
            }
            Dialog::EditorClose => {
                match overlay(
                    ctx,
                    "dlg",
                    order,
                    tr!("common.edit_playlist"),
                    size,
                    |ui| centered_text(ui, tr!("dlg.playlist_unsaved")),
                    &[(tr!("common.close_unsaved"), true), (tr!("common.no_cancel"), true)],
                ) {
                    Some(0) => {
                        close = true;
                        self.pl_editor = None;
                    }
                    Some(_) => close = true,
                    None => {}
                }
            }
            Dialog::SongEditorClose => {
                match overlay(
                    ctx,
                    "dlg",
                    order,
                    tr!("common.edit_song"),
                    size,
                    |ui| centered_text(ui, tr!("dlg.song_unsaved")),
                    &[(tr!("common.close_unsaved"), true), (tr!("common.no_cancel"), true)],
                ) {
                    Some(0) => {
                        close = true;
                        self.song_ed = None;
                    }
                    Some(_) => close = true,
                    None => {}
                }
            }
        }
        // a dialog action may have opened a new dialog (e.g. an error)
        if !close && self.dialog.is_none() {
            self.dialog = Some(d);
        }
    }

    fn new_song_dialog(&mut self) {
        let dir = self.lib().songs_dir();
        let mut name = tr!("songed.default_name").to_string();
        let mut n = 2;
        while dir.join(format!("{}.songprj", name)).exists() {
            name = format!("{} {}", tr!("songed.default_name"), n);
            n += 1;
        }
        self.dialog = Some(Dialog::NewSong { name, sr: 48000, bits: 24 });
    }

    /// write an empty .songprj, select it and open the song editor
    fn create_song(&mut self, name: &str, sr: u32, bits: u32) {
        let result = self.lib().create_song(name, sr, bits);
        let Ok(path) = result else {
            self.lib_ok(result);
            return;
        };
        self.refresh();
        self.select_song_path(&path);
        if let Sel::Song(i) = self.sel {
            self.open_song_editor(i);
        }
    }

    /// "New Multitrack Song": name, sample rate and resolution (Create / Cancel).
    fn new_song_dialog_ui(&mut self, ctx: &egui::Context, d: &mut Dialog) -> bool {
        let Dialog::NewSong { name, sr, bits } = d else { return true };
        let trimmed = name.trim().to_string();
        let exists = self.lib().songs_dir()
            .join(format!("{}.songprj", trimmed))
            .exists();
        let invalid = !valid_fat32_name(&trimmed);
        let ok = !invalid && !exists;
        let clicked = overlay(
            ctx,
            "dlg",
            egui::Order::Foreground,
            tr!("common.new_ml_song"),
            vec2(920.0, 340.0),
            |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    ui.label(RichText::new(tr!("dlg.new_song_name")).size(15.0));
                    ui.add_space(14.0);
                    let r = ui.add(egui::TextEdit::singleline(name).desired_width(600.0));
                    r.request_focus();
                    ui.add_space(8.0);
                    if invalid {
                        ui.label(RichText::new(tr!("dlg.song_name_invalid")).color(ORANGE));
                    } else if exists {
                        ui.label(RichText::new(tr!("dlg.song_exists")).color(ORANGE));
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.add_space((ui.available_width() - 420.0).max(0.0) / 2.0);
                        ui.label(tr!("dlg.sample_rate"));
                        ui.radio_value(sr, 44100, "44.1 kHz");
                        ui.radio_value(sr, 48000, "48 kHz");
                        ui.add_space(24.0);
                        ui.label(tr!("dlg.resolution"));
                        ui.radio_value(bits, 16, tr!("common.bits", n = 16));
                        ui.radio_value(bits, 24, tr!("common.bits", n = 24));
                    });
                });
            },
            &[(tr!("common.create"), ok), (tr!("common.cancel"), true)],
        );
        let enter = ok && ctx.input(|i| i.key_pressed(egui::Key::Enter));
        match clicked {
            Some(0) => {}
            Some(_) => return true,
            None if enter => {}
            None => return false,
        }
        let (sr, bits) = (*sr, *bits);
        self.create_song(&trimmed, sr, bits);
        true
    }

    /// "Song Name Change" / "Playlist Name Change" (Rename / Copy / Cancel).
    fn name_dialog(&mut self, ctx: &egui::Context, d: &mut Dialog) -> bool {
        let (is_song, index, name, copy_only) = match d {
            Dialog::SongName { index, name, copy_only } => (true, *index, name, *copy_only),
            Dialog::PlaylistName { index, name, copy_only } => (false, *index, name, *copy_only),
            _ => return true,
        };
        
        let current = if is_song {
            self.songs.get(index).map(|s| s.name.clone())
        } else {
            self.playlists.get(index).map(|p| p.name.clone())
        };
        let Some(current) = current else { return true };
        let trimmed = name.trim().to_string();
        let ext = if is_song { "songprj" } else { "playlistprj" };
        let dir = if is_song {
            self.lib().songs_dir()
        } else {
            self.lib().playlists_dir()
        };
        let exists = dir.join(format!("{}.{}", trimmed, ext)).exists();
        let invalid = !valid_fat32_name(&trimmed);
        let ok = !invalid && !exists && trimmed != current;
        let question = if copy_only {
            if is_song { tr!("dlg.copy_song_q", name = current) } else { tr!("dlg.copy_playlist_q", name = current) }
        } else {
            (if is_song { tr!("dlg.rename_song_q") } else { tr!("dlg.rename_playlist_q") }).to_string()
        };
        let buttons: Vec<(&str, bool)> = if copy_only {
            vec![(tr!("common.copy"), ok), (tr!("common.cancel"), true)]
        } else {
            vec![(tr!("common.rename"), ok), (tr!("common.copy"), ok), (tr!("common.cancel"), true)]
        };
        let clicked = overlay(
            ctx,
            "dlg",
            egui::Order::Foreground,
            if is_song { tr!("dlg.song_rename") } else { tr!("dlg.playlist_rename") },
            vec2(920.0, 300.0),
            |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(50.0);
                    ui.label(RichText::new(&question).size(15.0));
                    ui.add_space(14.0);
                    let r = ui.add(egui::TextEdit::singleline(name).desired_width(600.0));
                    r.request_focus();
                    ui.add_space(8.0);
                    if invalid {
                        ui.label(RichText::new(if is_song { tr!("dlg.song_name_invalid") } else { tr!("home.playlist_name_invalid") }).color(ORANGE));
                    } else if exists && trimmed != current {
                        ui.label(RichText::new(if is_song { tr!("dlg.song_exists") } else { tr!("dlg.playlist_exists") }).color(ORANGE));
                    }
                });
            },
            &buttons,
        );
        let action = match (clicked, copy_only) {
            (Some(0), true) => Some(true),
            (Some(0), false) => Some(false),
            (Some(1), false) => Some(true),
            (Some(_), _) => return true,
            (None, _) => None,
        };
        if let Some(copy) = action {
            if is_song {
                self.song_name_change(index, &trimmed, copy);
            } else {
                self.playlist_name_change(index, &trimmed, copy);
            }
            return true;
        }
        false
    }
}

/// longest delay the playlist editor offers (seconds)
const MAX_START_DELAY: u32 = 600;

fn start_mode_help(m: StartMode) -> &'static str {
    match m {
        StartMode::Button => tr!("mode.button_tip"),
        StartMode::Timer => tr!("mode.timer_tip"),
        StartMode::NoWait => tr!("mode.nowait_tip"),
    }
}

/// start mode symbol: a pause sign for "wait for button press" (as in
/// uTool2), a play sign for "no wait", a clock with the seconds for
/// "wait for timer" and "?" for a value lp16-tool does not know
fn paint_mode(p: &egui::Painter, c: egui::Pos2, mode: &str, delay: &str) {
    match StartMode::parse(mode) {
        Some(StartMode::Button) => {
            for dx in [-3.5f32, 3.5] {
                p.rect_filled(Rect::from_center_size(pos2(c.x + dx, c.y), vec2(4.5, 14.0)), 0.0, TEXT);
            }
        }
        Some(StartMode::NoWait) => {
            p.add(egui::Shape::convex_polygon(
                vec![pos2(c.x - 5.0, c.y - 7.0), pos2(c.x + 6.0, c.y), pos2(c.x - 5.0, c.y + 7.0)],
                ORANGE,
                Stroke::NONE,
            ));
        }
        Some(StartMode::Timer) => {
            let cc = pos2(c.x - 8.0, c.y);
            p.circle_stroke(cc, 6.5, Stroke::new(1.5_f32, ORANGE));
            p.line_segment([cc, pos2(cc.x, cc.y - 4.5)], Stroke::new(1.5_f32, ORANGE));
            p.line_segment([cc, pos2(cc.x + 3.5, cc.y)], Stroke::new(1.5_f32, ORANGE));
            p.text(pos2(c.x + 1.0, c.y), Align2::LEFT_CENTER, format!("{}s", delay_seconds(delay)), FontId::proportional(11.0), ORANGE);
        }
        None => {
            p.text(c, Align2::CENTER_CENTER, "?", FontId::proportional(14.0), ORANGE);
        }
    }
}

/// Home > playlist: a running time in a fixed-width column.
fn time_col(ui: &mut Ui, text: &str, color: Color32) {
    ui.add_sized(vec2(52.0, 16.0), egui::Label::new(RichText::new(text).font(FontId::proportional(11.5)).color(color)));
}

/// the start mode symbol of a list row, with its name as tooltip
fn mode_widget(ui: &mut Ui, mode: &str, delay: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(52.0, 16.0), Sense::hover());
    paint_mode(ui.painter(), rect.center(), mode, delay);
    resp.on_hover_text(start_mode_text(mode, delay))
}

// ---------------------------------------------------------------------------
// Edit Multitrack Song
// ---------------------------------------------------------------------------

const SLOTS: usize = 16;

/// One entry of the "Audio Files" browser.
struct AudioEntry {
    name: String,
    path: String,
    dir: bool,
    info: Option<WavInfo>,
}

/// "Edit Multitrack Song" overlay state (a working copy of the .songprj).
struct SongEditor {
    path: String,
    /// the project as loaded; save() writes the slots and the MIDI file into it
    prj: SongProject,
    name: String,
    /// name on the printed setlist (stored in the `.songinfo`); without a
    /// stored one the name made from the song name, as editable text
    print_title: String,
    /// `print_title` when the editor was opened (only a change is stored)
    print_title_initial: String,
    sr: u32,
    bits: u32,
    /// slot n (0-based) = output n+1
    slots: Vec<Option<ProjectTrack>>,
    slot_info: Vec<Option<WavInfo>>,
    slot_check: Vec<bool>,
    /// start of a Shift range in the Tracks list
    slot_anchor: Option<usize>,
    slot_rects: Vec<Rect>,
    /// MIDI file as stored ({$AudioLib}\...), empty = none
    midi: String,
    midi_len: Option<f64>,
    folder: String,
    files: Vec<AudioEntry>,
    file_check: Vec<bool>,
    anchor: Option<usize>,
    /// WAV files being dragged onto the slots
    drag: Option<Vec<String>>,
    dirty: bool,
    message: Option<String>,
    /// the Mixer button was clicked (handled by `song_editor`)
    open_mixer: bool,
}

/// Compare names like Windows Explorer does ("-2-" before "-10-").
/// Sub folders and WAV files of a folder, sorted like the Explorer.
fn list_audio(folder: &str) -> Vec<AudioEntry> {
    let mut dirs = Vec::new();
    let mut wavs = Vec::new();
    if let Ok(rd) = fs::read_dir(folder) {
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            let path = p.to_string_lossy().into_owned();
            if p.is_dir() {
                dirs.push(AudioEntry { name, path, dir: true, info: None });
            } else if name.to_lowercase().ends_with(".wav") && !wav_tail::is_backup_wav(&p) {
                let info = WavInfo::read(&path).ok();
                wavs.push(AudioEntry { name, path, dir: false, info });
            }
        }
    }
    dirs.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    wavs.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    dirs.extend(wavs);
    dirs
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

impl SongEditor {
    fn load(path: &str, audio_lib: &str) -> Result<Self, String> {
        // an unreadable project is not opened: saving would write it empty
        let prj = SongProject::load(path, audio_lib).map_err(|e| e.to_string())?;
        let mut slots = vec![None; SLOTS];
        for t in &prj.tracks {
            if (0..SLOTS as i32).contains(&t.channel) {
                slots[t.channel as usize] = Some(t.clone());
            }
        }
        let folder = slots
            .iter()
            .flatten()
            .filter(|t| !t.audiofile.is_empty())
            .filter_map(|t| {
                Path::new(&prj.wav(t))
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .find(|d| Path::new(d).is_dir())
            .unwrap_or_else(|| audio_lib.to_string());
        let mut ed = SongEditor {
            path: path.to_string(),
            name: prj.name.clone(),
            print_title: String::new(),
            print_title_initial: String::new(),
            sr: prj.sampling_rate,
            bits: prj.resolution,
            midi: prj.midi_file.clone(),
            prj,
            slots,
            slot_info: vec![None; SLOTS],
            slot_check: vec![false; SLOTS],
            slot_anchor: None,
            slot_rects: vec![Rect::NOTHING; SLOTS],
            midi_len: None,
            folder: String::new(),
            files: Vec::new(),
            file_check: Vec::new(),
            anchor: None,
            drag: None,
            dirty: false,
            message: None,
            open_mixer: false,
        };
        let stored = song_info::load(Path::new(path)).print_title;
        ed.print_title = if stored.is_empty() { song_info::default_print_title(&ed.name) } else { stored };
        ed.print_title_initial = ed.print_title.clone();
        ed.open_folder(folder);
        ed.recalc(audio_lib);
        Ok(ed)
    }

    fn open_folder(&mut self, folder: String) {
        self.files = list_audio(&folder);
        self.file_check = vec![false; self.files.len()];
        self.anchor = None;
        self.folder = folder;
    }

    /// re-read the WAV/MIDI facts after a change
    fn recalc(&mut self, audio_lib: &str) {
        self.slot_info = self
            .slots
            .iter()
            .map(|s| {
                s.as_ref()
                    .filter(|t| !t.audiofile.is_empty())
                    .and_then(|t| WavInfo::read(&resolve(&t.audiofile, audio_lib)).ok())
            })
            .collect();
        self.midi_len = if self.midi.is_empty() {
            None
        } else {
            midi_duration(&resolve(&self.midi, audio_lib))
        };
    }

    fn wav_len(&self) -> f64 {
        self.slot_info.iter().flatten().map(|i| i.seconds()).fold(0.0, f64::max)
    }

    /// does a WAV fit the song (mono, same sampling rate and resolution)?
    fn fits(&self, i: &WavInfo) -> bool {
        i.channels == 1 && i.rate == self.sr && i.bits as u32 == self.bits
    }

    /// Put WAV files into the slots, the first one at `start`, the others
    /// into the following slots (occupied slots are replaced).
    fn assign(&mut self, start: usize, files: &[String], audio_lib: &str) {
        let mut rejected = Vec::new();
        let mut overflow = 0;
        let mut slot = start;
        for f in files {
            match WavInfo::read(f) {
                Ok(i) if self.fits(&i) => {
                    if slot >= SLOTS {
                        overflow += 1;
                        continue;
                    }
                    self.slots[slot] = Some(ProjectTrack::new(slot as i32, f, audio_lib));
                    self.slot_check[slot] = false;
                    self.dirty = true;
                    slot += 1;
                }
                Ok(i) => rejected.push(tr!("songed.rejected", name = file_name(f), ch = i.channels, rate = fmt_sr(i.rate), bits = i.bits)),
                Err(_) => rejected.push(format!("{}  ({})", file_name(f), tr!("common.not_usable_wav"))),
            }
        }
        self.recalc(audio_lib);
        let mut msg = String::new();
        if !rejected.is_empty() {
            msg += &tr!("songed.needs_mono", rate = fmt_sr(self.sr), bits = self.bits, list = rejected.join("\n"));
        }
        if overflow > 0 {
            if !msg.is_empty() {
                msg += "\n\n";
            }
            msg += &tr!("songed.overflow", n = overflow);
        }
        if !msg.is_empty() {
            self.message = Some(msg);
        }
    }

    fn first_free(&self) -> usize {
        self.slots.iter().position(|s| s.is_none()).unwrap_or(SLOTS)
    }

    fn set_midi(&mut self, full: &str, audio_lib: &str) {
        self.midi = unresolve(full, audio_lib);
        self.dirty = true;
        self.recalc(audio_lib);
        if self.midi_len.is_none() {
            self.message = Some(tr!("songed.midi_unreadable", name = file_name(full)));
        }
    }

    /// files dropped from the Explorer: WAVs onto the slot under the mouse
    /// (or the first free one), a .mid becomes the MIDI file, a folder is
    /// opened in the browser
    fn os_drop(&mut self, ctx: &egui::Context, paths: &[String], audio_lib: &str) {
        let pos = ctx.input(|i| i.pointer.hover_pos());
        let start = pos
            .and_then(|p| self.slot_rects.iter().position(|r| r.contains(p)))
            .unwrap_or_else(|| self.first_free());
        let mut wavs: Vec<String> = paths.iter().filter(|p| p.to_lowercase().ends_with(".wav")).cloned().collect();
        wavs.sort_by(|a, b| natural_cmp(&file_name(a), &file_name(b)));
        if !wavs.is_empty() {
            self.assign(start, &wavs, audio_lib);
        }
        if let Some(m) = paths.iter().find(|p| {
            let l = p.to_lowercase();
            l.ends_with(".mid") || l.ends_with(".midi")
        }) {
            self.set_midi(m, audio_lib);
        }
        if let Some(d) = paths.iter().find(|p| Path::new(p).is_dir()) {
            self.open_folder(d.clone());
        }
    }

    /// write the .songprj; keeps name, timestamp, markers and mixer values
    fn save(&mut self, lib: &ProjectLibrary) -> Result<(), String> {
        let unreadable = self
            .slots
            .iter()
            .zip(&self.slot_info)
            .any(|(s, i)| s.as_ref().map(|t| !t.audiofile.is_empty()).unwrap_or(false) && i.is_none());
        let mut prj = self.prj.clone();
        // a WAV that can't be read keeps the stored length
        let mut duration = self.wav_len();
        if unreadable {
            duration = duration.max(prj.duration_seconds);
        }
        // a MIDI file that can't be read keeps its stored length, if unchanged
        prj.midi_duration = if self.midi.is_empty() {
            0.0
        } else if let Some(l) = self.midi_len {
            l
        } else if prj.midi_file == self.midi {
            prj.midi_duration
        } else {
            0.0
        };
        prj.tracks = self.slots.iter().flatten().cloned().collect();
        prj.duration_seconds = duration;
        prj.midi_file = self.midi.clone();
        // the Song Mix in the file wins (see ProjectLibrary::save_song)
        let prj = lib.save_song(&prj)?;
        self.prj = prj;
        let title = self.print_title.trim().to_string();
        if title != self.print_title_initial.trim() {
            lib.save_song_info(Path::new(&self.path), &SongFacts { print_title: title.clone() })?;
            self.print_title_initial = title.clone();
        }
        self.print_title = title;
        self.dirty = false;
        Ok(())
    }
}

/// folder symbol (the emoji is missing in egui's fonts)
fn paint_folder(p: &egui::Painter, c: egui::Pos2, color: Color32) {
    let st = Stroke::new(1.3_f32, color);
    let body = Rect::from_center_size(c + vec2(0.0, 1.5), vec2(16.0, 11.0));
    p.rect_stroke(body, 1.0, st);
    p.line_segment([pos2(body.left(), body.top()), pos2(body.left(), body.top() - 3.0)], st);
    p.line_segment([pos2(body.left(), body.top() - 3.0), pos2(body.left() + 6.0, body.top() - 3.0)], st);
    p.line_segment([pos2(body.left() + 6.0, body.top() - 3.0), pos2(body.left() + 8.0, body.top())], st);
}

/// audio file symbol: a small sine wave
fn paint_wave(p: &egui::Painter, c: egui::Pos2, color: Color32) {
    let pts: Vec<egui::Pos2> = (0..=16)
        .map(|i| {
            let t = i as f32 / 16.0;
            pos2(c.x - 8.0 + 16.0 * t, c.y - 3.5 * (t * std::f32::consts::TAU).sin())
        })
        .collect();
    p.add(egui::Shape::line(pts, Stroke::new(1.3_f32, color)));
}

/// frameless folder button ("open ...")
pub(crate) fn folder_btn(ui: &mut Ui, tip: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 3.0, HOVER);
    }
    paint_folder(ui.painter(), rect.center(), if resp.hovered() { ORANGE } else { TEXT });
    resp.on_hover_text(tip).clicked()
}

/// small frameless icon button (reload, trash)
fn icon_btn(ui: &mut Ui, icon: &str, tip: &str, enabled: bool) -> bool {
    ui.add_enabled(enabled, egui::Button::new(RichText::new(icon).size(16.0)).frame(false))
        .on_hover_text(tip)
        .clicked()
}

fn path_field(ui: &mut Ui, text: &str, color: Color32) {
    egui::Frame::none().fill(FIELD).inner_margin(Margin::symmetric(5.0, 3.0)).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            ui.add(egui::Label::new(RichText::new(text).color(color)).truncate(true));
        });
    });
}

fn song_editor_body(ui: &mut Ui, ed: &mut SongEditor, audio_lib: &str, frame: &eframe::Frame) {
    panel_tab(ui, tr!("songed.settings"), false);
    panel_body(ui, false, |ui| {
        ui.horizontal(|ui| {
            ui.label(format!("       {}", tr!("pled.location")));
            ui.label("LP16\\Multitrack Songs");
        });
        ui.horizontal(|ui| {
            ui.label(format!("   {}", tr!("songed.name")));
            let mut n = ed.name.clone();
            ui.add_enabled(false, egui::TextEdit::singleline(&mut n).desired_width(ui.available_width() - 20.0));
        });
        ui.horizontal(|ui| {
            ui.label(format!("   {}", tr!("common.print_title")));
            let edit = egui::TextEdit::singleline(&mut ed.print_title).desired_width(ui.available_width() - 20.0);
            if ui.add(edit).on_hover_text(tr!("songed.print_title_hint")).changed() {
                ed.dirty = true;
            }
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr!("songed.synched")).color(DIM));
            ui.label(RichText::new(tr!("songed.standalone")).color(DIM));
            ui.add_space(200.0);
            ui.label(tr!("songed.rate"));
            ui.label(fmt_sr(ed.sr));
            ui.add_space(200.0);
            ui.label(tr!("dlg.resolution"));
            ui.label(tr!("common.bits", n = ed.bits));
        });
    });
    ui.add_space(12.0);
    let h = ui.available_height();
    let w = ui.available_width();
    let half = (w - 30.0) / 2.0;
    let top = ui.cursor().min;
    let pointer = ui.input(|i| i.pointer.hover_pos());
    let released = ui.input(|i| i.pointer.any_released());

    // ---- Audio Files ----
    ui.allocate_ui_at_rect(Rect::from_min_size(top, vec2(half, h)), |ui| {
        panel_tab(ui, tr!("songed.audio_files"), false);
        panel_body(ui, true, |ui| {
            ui.horizontal(|ui| {
                if folder_btn(ui, tr!("songed.open_folder")) {
                    let start = if Path::new(&ed.folder).is_dir() { ed.folder.clone() } else { audio_lib.to_string() };
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_title(tr!("songed.audio_files"))
                        .set_directory(&start)
                        .set_parent(frame)
                        .pick_folder()
                    {
                        ed.open_folder(dir.to_string_lossy().into_owned());
                    }
                }
                if icon_btn(ui, "🔄", tr!("songed.reload"), true) {
                    let f = ed.folder.clone();
                    ed.open_folder(f);
                }
                path_field(ui, &ed.folder, TEXT);
            });
            ui.add_space(4.0);
            // header: select all, File Name, Length
            let wav_idx: Vec<usize> = (0..ed.files.len()).filter(|&i| !ed.files[i].dir).collect();
            let n_checked = wav_idx.iter().filter(|&&i| ed.file_check[i]).count();
            let all_state = if n_checked == 0 {
                Check::Off
            } else if n_checked == wav_idx.len() {
                Check::On
            } else {
                Check::Partial
            };
            let (hr, hresp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
            let cb = Rect::from_center_size(pos2(hr.left() + 13.0, hr.center().y), vec2(14.0, 14.0));
            {
                let p = ui.painter_at(hr);
                paint_check(&p, cb, all_state, !wav_idx.is_empty());
                p.text(pos2(hr.center().x, hr.center().y), Align2::CENTER_CENTER, tr!("songed.file_name"), FontId::proportional(12.0), DIM);
                p.text(pos2(hr.right() - 30.0, hr.center().y), Align2::CENTER_CENTER, tr!("common.length"), FontId::proportional(12.0), DIM);
            }
            if hresp.clicked() && hresp.interact_pointer_pos().map(|q| cb.expand(3.0).contains(q)).unwrap_or(false) {
                let on = all_state != Check::On;
                for &i in &wav_idx {
                    ed.file_check[i] = on;
                }
            }
            let parent = Path::new(&ed.folder).parent().map(|p| p.to_string_lossy().into_owned());
            let mut open_dir: Option<String> = None;
            let mut to_slot: Option<String> = None;
            egui::ScrollArea::vertical().id_source("audio_files").auto_shrink([false; 2]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                if let Some(parent) = &parent {
                    if tree_row(ui, 0, None, None, " ", tr!("songed.up"), false, false, |p, r| paint_folder(p, pos2(r.left() + 33.0, r.center().y), TEXT)).double {
                        open_dir = Some(parent.clone());
                    }
                }
                let modifiers = ui.input(|i| i.modifiers);
                for i in 0..ed.files.len() {
                    let e = &ed.files[i];
                    if e.dir {
                        if tree_row(ui, 0, None, None, " ", &e.name, false, false, |p, r| paint_folder(p, pos2(r.left() + 33.0, r.center().y), TEXT)).double {
                            open_dir = Some(e.path.clone());
                        }
                        continue;
                    }
                    let ok = e.info.map(|inf| ed.fits(&inf)).unwrap_or(false);
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click_and_drag());
                    let p = ui.painter_at(rect);
                    if ed.file_check[i] {
                        p.rect_filled(rect, 0.0, SEL);
                    } else if resp.hovered() {
                        p.rect_filled(rect, 0.0, HOVER);
                    }
                    let cy = rect.center().y;
                    let cb = Rect::from_center_size(pos2(rect.left() + 13.0, cy), vec2(14.0, 14.0));
                    paint_check(&p, cb, if ed.file_check[i] { Check::On } else { Check::Off }, true);
                    let color = if ok { TEXT } else { DIM };
                    paint_wave(&p, pos2(rect.left() + 39.0, cy), color);
                    p.text(pos2(rect.left() + 52.0, cy), Align2::LEFT_CENTER, &e.name, FontId::proportional(14.0), color);
                    let len = e.info.map(|inf| fmt_len(inf.seconds())).unwrap_or_else(|| "-".into());
                    p.text(pos2(rect.right() - 30.0, cy), Align2::CENTER_CENTER, len, FontId::proportional(12.5), color);
                    let resp = if ok {
                        resp
                    } else {
                        let why = match e.info {
                            Some(inf) => tr!("songed.wrong_format", ch = inf.channels, rate = fmt_sr(inf.rate), bits = inf.bits, need_rate = fmt_sr(ed.sr), need_bits = ed.bits),
                            None => tr!("common.not_usable_wav").into(),
                        };
                        resp.on_hover_text(why)
                    };
                    if resp.drag_started() {
                        let files: Vec<String> = if ed.file_check[i] {
                            (0..ed.files.len())
                                .filter(|&j| ed.file_check[j] && !ed.files[j].dir)
                                .map(|j| ed.files[j].path.clone())
                                .collect()
                        } else {
                            vec![ed.files[i].path.clone()]
                        };
                        ed.drag = Some(files);
                    } else if resp.double_clicked() {
                        to_slot = Some(ed.files[i].path.clone());
                    } else if resp.clicked() {
                        let in_cb = resp.interact_pointer_pos().map(|q| cb.expand(3.0).contains(q)).unwrap_or(false);
                        if in_cb || modifiers.command {
                            ed.file_check[i] = !ed.file_check[i];
                        } else if modifiers.shift && ed.anchor.is_some() {
                            let a = ed.anchor.unwrap();
                            let (lo, hi) = (a.min(i), a.max(i));
                            for j in lo..=hi {
                                if !ed.files[j].dir {
                                    ed.file_check[j] = true;
                                }
                            }
                        } else {
                            for c in ed.file_check.iter_mut() {
                                *c = false;
                            }
                            ed.file_check[i] = true;
                        }
                        if !modifiers.shift {
                            ed.anchor = Some(i);
                        }
                    }
                }
            });
            if let Some(d) = open_dir {
                ed.open_folder(d);
            }
            if let Some(f) = to_slot {
                let free = ed.first_free();
                ed.assign(free, &[f], audio_lib);
            }
        });
    });

    // ---- Tracks ----
    let mut drop_at: Option<usize> = None;
    ui.allocate_ui_at_rect(Rect::from_min_size(top + vec2(half + 30.0, 0.0), vec2(half, h - 100.0)), |ui| {
        panel_tab(ui, tr!("import.col_tracks"), false);
        panel_body(ui, true, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let used: Vec<usize> = (0..SLOTS).filter(|&i| ed.slots[i].is_some()).collect();
            let n_checked = used.iter().filter(|&&i| ed.slot_check[i]).count();
            let all_state = if n_checked == 0 {
                Check::Off
            } else if n_checked == used.len() {
                Check::On
            } else {
                Check::Partial
            };
            let mut delete = false;
            ui.horizontal(|ui| {
                let (cb, resp) = ui.allocate_exact_size(vec2(20.0, ROW_H), Sense::click());
                paint_check(ui.painter(), Rect::from_center_size(cb.center(), vec2(14.0, 14.0)), all_state, !used.is_empty());
                if resp.clicked() && !used.is_empty() {
                    let on = all_state != Check::On;
                    for &i in &used {
                        ed.slot_check[i] = on;
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(8.0);
                    // the player loads the saved song, so only without unsaved changes
                    if ui
                        .add_enabled(!ed.dirty, egui::Button::new(tr!("common.mixer")).frame(false))
                        .on_hover_text(tr!("songed.mixer_tip"))
                        .clicked()
                    {
                        ed.open_mixer = true;
                    }
                    ui.add_space(20.0);
                    delete = ui
                        .add_enabled(n_checked > 0, egui::Button::new(tr!("common.delete")).frame(false))
                        .on_hover_text(tr!("songed.delete_tip"))
                        .clicked();
                });
            });
            if n_checked > 0 && delete_key(ui) {
                delete = true;
            }
            if delete {
                ed.slot_anchor = None;
                for i in 0..SLOTS {
                    if ed.slot_check[i] {
                        ed.slots[i] = None;
                        ed.slot_check[i] = false;
                        ed.dirty = true;
                    }
                }
                ed.recalc(audio_lib);
            }
            ui.add_space(4.0);
            let hovered_slot = ed
                .drag
                .as_ref()
                .and(pointer)
                .and_then(|p| ed.slot_rects.iter().position(|r| r.contains(p)));
            let n_drag = ed.drag.as_ref().map(|d| d.len()).unwrap_or(0);
            for i in 0..SLOTS {
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                ed.slot_rects[i] = rect;
                let p = ui.painter_at(rect);
                let target = hovered_slot.map(|s| i >= s && i < s + n_drag).unwrap_or(false);
                if target {
                    p.rect_filled(rect, 0.0, Color32::from_rgb(0x5a, 0x40, 0x30));
                    p.rect_stroke(rect.shrink(0.5), 0.0, Stroke::new(1.0_f32, ORANGE));
                } else if ed.slot_check[i] {
                    p.rect_filled(rect, 0.0, SEL);
                } else if resp.hovered() {
                    p.rect_filled(rect, 0.0, HOVER);
                }
                let cy = rect.center().y;
                let cb = Rect::from_center_size(pos2(rect.left() + 10.0, cy), vec2(14.0, 14.0));
                paint_check(&p, cb, if ed.slot_check[i] { Check::On } else { Check::Off }, ed.slots[i].is_some());
                p.text(pos2(rect.left() + 22.0, cy), Align2::LEFT_CENTER, format!("{}", i + 1), FontId::proportional(14.0), TEXT);
                if let Some(t) = &ed.slots[i] {
                    let name = if t.name.is_empty() { stem(&t.audiofile) } else { t.name.clone() };
                    p.text(pos2(rect.left() + 52.0, cy), Align2::LEFT_CENTER, &name, FontId::proportional(14.0), TEXT);
                    let missing = !t.audiofile.is_empty() && ed.slot_info[i].is_none();
                    if missing {
                        paint_warn(&p, rect);
                    }
                    let full = resolve(&t.audiofile, audio_lib);
                    let resp = resp.on_hover_text(if missing { format!("{}:\n{}", tr!("common.missing_wav"), full) } else { full });
                    if resp.clicked() {
                        let in_cb = resp.interact_pointer_pos().map(|q| cb.expand(3.0).contains(q)).unwrap_or(false);
                        let m = ui.input(|i| i.modifiers);
                        let used: Vec<bool> = ed.slots.iter().map(|s| s.is_some()).collect();
                        list_click(&mut ed.slot_check, &mut ed.slot_anchor, i, m, in_cb, |j| used[j]);
                    }
                }
            }
            if released && ed.drag.is_some() {
                drop_at = hovered_slot;
            }
        });
    });
    if released {
        if let Some(files) = ed.drag.take() {
            if let Some(start) = drop_at {
                ed.assign(start, &files, audio_lib);
            }
        }
    }

    // ---- MIDI File ----
    ui.allocate_ui_at_rect(Rect::from_min_size(top + vec2(half + 30.0, h - 90.0), vec2(half, 90.0)), |ui| {
        panel_tab(ui, tr!("songed.midi_file"), false);
        panel_body(ui, true, |ui| {
            let full = if ed.midi.is_empty() { String::new() } else { resolve(&ed.midi, audio_lib) };
            ui.horizontal(|ui| {
                if folder_btn(ui, tr!("songed.choose_midi")) {
                    let start = Path::new(&full)
                        .parent()
                        .filter(|d| d.is_dir())
                        .map(|d| d.to_string_lossy().into_owned())
                        .unwrap_or_else(|| ed.folder.clone());
                    if let Some(f) = rfd::FileDialog::new()
                        .set_title(tr!("songed.midi_file"))
                        .set_directory(&start)
                        .add_filter("MIDI", &["mid", "midi"])
                        .set_parent(frame)
                        .pick_file()
                    {
                        ed.set_midi(&f.to_string_lossy(), audio_lib);
                    }
                }
                let missing = !full.is_empty() && ed.midi_len.is_none();
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if icon_btn(ui, "🗑", tr!("songed.remove_midi"), !ed.midi.is_empty()) {
                        ed.midi.clear();
                        ed.dirty = true;
                        ed.recalc(audio_lib);
                    }
                    path_field(ui, if full.is_empty() { "-" } else { &full }, if missing { ORANGE } else { TEXT });
                });
            });
            let len = ed.wav_len().max(ed.midi_len.unwrap_or(0.0));
            ui.horizontal(|ui| {
                ui.label(tr!("songed.duration", time = fmt_len(len)));
                if !full.is_empty() && ed.midi_len.is_none() {
                    badge(ui, tr!("common.missing_midi"));
                }
            });
        });
    });
    ui.allocate_rect(Rect::from_min_size(top, vec2(w, h)), Sense::hover());
}

/// Run the GUI. Settings are written to lp16-tool.toml.
pub fn run_gui(config: Config) {
    i18n::set_lang(Lang::from_code(&config.language));
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1500.0, 950.0])
            .with_min_inner_size([1100.0, 700.0])
            .with_title("lp16-tool"),
        ..Default::default()
    };
    let _ = eframe::run_native(
        "lp16-tool",
        native_options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(visuals());
            Box::new(Lp16App::new(config))
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::move_checked;

    #[test]
    fn move_checked_block() {
        let mut v = vec!['a', 'b', 'c', 'd', 'e'];
        // b and d to the end
        let c = move_checked(&mut v, &[false, true, false, true, false], 5);
        assert_eq!(v, vec!['a', 'c', 'e', 'b', 'd']);
        assert_eq!(c, vec![false, false, false, true, true]);
        // b and d to the front
        let c = move_checked(&mut v, &c, 0);
        assert_eq!(v, vec!['b', 'd', 'a', 'c', 'e']);
        assert_eq!(c, vec![true, true, false, false, false]);
        // block into the gap between c and e
        let c = move_checked(&mut v, &c, 4);
        assert_eq!(v, vec!['a', 'c', 'b', 'd', 'e']);
        assert_eq!(c, vec![false, false, true, true, false]);
        // dropping onto its own position changes nothing
        let c2 = move_checked(&mut v, &c, 3);
        assert_eq!(v, vec!['a', 'c', 'b', 'd', 'e']);
        assert_eq!(c2, c);
    }
}

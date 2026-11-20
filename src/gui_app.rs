//! The lp16-tool GUI (egui/eframe) - a rebuild of the uTool2 main window.
//!
//! Layout, colours and wording follow the screenshots of the original in
//! uTool2 V2.0.062 (kept locally in _private/docs/utool2-screens):
//! - seven tabs (Home, Rehearsal, Export, Import, Edit Target, Settings, Support);
//!   Rehearsal is new (the player), see gui_rehearsal.rs
//! - two-column pages: explorer tree on the left, details on the right
//! - editors and dialogs are overlays inside the main window: black
//!   backdrop, orange title, dark rounded box, orange-framed buttons.
//!
//! Everything goes through the same lp16_tool library that the CLI uses.

use crate::config_path;
use crate::gui_audio::AudioPanel;
use crate::gui_rehearsal::RehearsalPanel;
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, FontId, Layout, Margin, Rect, RichText, Sense,
    Stroke, Ui,
};
use lp16_tool::audio_out::AudioSettings;
use lp16_tool::mix::MixSaver;
use lp16_tool::project_lib::ProjectLibrary;
use lp16_tool::sync::{self, Action, ExportMode, Progress};
use lp16_tool::{
    import_songs, init_folders,
    library_songs, midi_duration, natural_cmp, newest_version, scan_import, ImportItem, ImportStatus, LibrarySong,
    now_ms, resolve, save_config, save_mixes,
    plain_tag, xml_unescape, unresolve, valid_fat32_name, Config,
    PlaylistSong, ProjectTrack, SongProject, WavInfo, STICK_WAV_HEADER_LEN,
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

// ---------------------------------------------------------------------------
// data
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Home,
    Rehearsal,
    Export,
    Import,
    EditTarget,
    Settings,
    Support,
}

const TABS: [(Tab, &str); 7] = [
    (Tab::Home, "Home"),
    (Tab::Rehearsal, "Rehearsal"),
    (Tab::Export, "Export"),
    (Tab::Import, "Import"),
    (Tab::EditTarget, "Edit Target"),
    (Tab::Settings, "Settings"),
    (Tab::Support, "Support"),
];

struct TrackInfo {
    num: i32,
    name: String,
    wav: String,
    exists: bool,
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
    size: u64,
    modified: i64,
}

impl SongInfo {
    fn warn(&self) -> bool {
        self.tracks.iter().any(|t| !t.exists) || (!self.midi_full.is_empty() && !self.midi_exists)
    }
    fn format_line(&self) -> String {
        format!("{} ch - {}-bit", self.tracks.len(), self.bits)
    }
}

/// One song entry of a playlist.
#[derive(Clone)]
struct PlSong {
    /// full path of the .songprj
    prj: String,
    mode: String,
    delay: String,
}

struct PlaylistInfo {
    name: String,
    songs: Vec<PlSong>,
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
    songs: Vec<PlSong>,
    checked: Vec<bool>,
    dirty: bool,
    avail_open: bool,
    avail_sel: Option<usize>,
    /// start of a Shift range in the Playlist Content
    anchor: Option<usize>,
    /// what is being dragged onto the Playlist Content
    drag: Option<PlDrag>,
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

struct EtDetails {
    title: String,
    lines: Vec<(String, String)>,
    list_title: String,
    list: Vec<String>,
    play_songs: bool,
}

pub struct Lp16App {
    config: Config,
    tab: Tab,
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
    // settings: the "Audio output" area
    audio: AudioPanel,
}

// ---------------------------------------------------------------------------
// helpers: paths, formatting, Windows bits
// ---------------------------------------------------------------------------

pub(crate) fn key(path: &str) -> String {
    path.to_lowercase()
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

fn fmt_len(sec: f64) -> String {
    let s = sec.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

fn fmt_size(b: u64) -> String {
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

fn fmt_date(ms: i64) -> String {
    if ms <= 0 {
        return "-".into();
    }
    let (y, mo, d, h, mi) = local_time(ms);
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
fn local_time(ms: i64) -> (i64, u32, u32, u32, u32) {
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
fn local_time(ms: i64) -> (i64, u32, u32, u32, u32) {
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
fn disk_free(path: &str) -> Option<u64> {
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
fn disk_free(_path: &str) -> Option<u64> {
    None
}


fn xml_all(xml: &str, name: &str) -> Vec<String> {
    let open = format!("<{}>", name);
    let close = format!("</{}>", name);
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(p) = rest.find(&open) {
        let s = p + open.len();
        let Some(e) = rest[s..].find(&close) else { break };
        out.push(xml_unescape(&rest[s..s + e]));
        rest = &rest[s + e + close.len()..];
    }
    out
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
                wav,
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
    if s.warn() {
        paint_warn(p, rect);
    }
}

fn paint_mark(p: &egui::Painter, rect: Rect, mark: Mark) {
    let text = match mark {
        Mark::None => return,
        Mark::Skip => "Skip",
        Mark::Quick => "Quick Export",
        Mark::Overwrite => "Overwrite",
        Mark::New => "New Export",
        Mark::Force => "Force Overwrite",
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
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
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
                .on_hover_text("Refresh")
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
fn overlay(
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
        for k in ["h:root", "t:rec", "t:pl", "e:rec", "e:pl", "x:pl"] {
            open.insert(k.to_string());
        }
        let mut app = Lp16App {
            config,
            tab: Tab::Home,
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
            audio: AudioPanel::default(),
        };
        app.rehearsal.set_settings(AudioSettings::from(&app.config));
        app.rehearsal.set_mix(app.config.mixes.active().params());
        app.refresh();
        app
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
        self.songs = lib.song_files().iter().map(|p| load_song(p, &audio_lib)).collect();
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
                size: fs::metadata(&pl.path).map(|m| m.len()).unwrap_or(0),
                modified: mtime(&pl.path),
            })
            .collect();
        match self.sel {
            Sel::Song(i) if i >= self.songs.len() => self.sel = Sel::SongsFolder,
            Sel::Playlist(i) if i >= self.playlists.len() => self.sel = Sel::PlaylistsFolder,
            _ => {}
        }
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
                self.message("Error", e);
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
            self.rehearsal.song_changed(&self.songs[index].path);
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
            self.refresh();
            self.select_playlist_name(new_name);
        }
    }

    fn delete_song(&mut self, index: usize) {
        self.rehearsal.song_changed(&self.songs[index].path);
        let path = PathBuf::from(&self.songs[index].path);
        match self.lib().delete_song(&path) {
            Ok(()) => {
                self.sel = Sel::SongsFolder;
                self.refresh();
            }
            Err(e) => self.message("Delete Song", e),
        }
    }

    fn delete_playlist(&mut self, index: usize) {
        let name = self.playlists[index].name.clone();
        let result = self.lib().delete_playlist(&name);
        if self.lib_ok(result) {
            self.sel = Sel::PlaylistsFolder;
            self.refresh();
        }
    }

    fn open_playlist_editor(&mut self, index: Option<usize>) {
        let (name, songs) = match index {
            Some(i) => (self.playlists[i].name.clone(), self.playlists[i].songs.clone()),
            None => ("New Playlist".to_string(), Vec::new()),
        };
        self.pl_editor = Some(PlaylistEditor {
            index,
            name,
            checked: vec![false; songs.len()],
            songs,
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
        let songs = ed.songs.clone();
        let index = ed.index;
        if !valid_fat32_name(&name) {
            self.message("Playlist Name", "The new Playlist Name contains invalid characters.");
            return;
        }
        let new_path = self.lib().playlist_path(&name);
        let same_name = index.map(|i| self.playlists[i].name == name).unwrap_or(false);
        if !same_name && new_path.exists() {
            self.message("Playlist Name", format!("A Playlist \"{}\" already exists.", name));
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
            (Some(i), false, Some(copy)) => lib.save_playlist_as(&self.playlists[i].name, &name, &songs, copy),
            (None, _, _) => lib.create_playlist(&name, &songs),
            _ => lib.save_playlist(&name, &songs),
        };
        if !self.lib_ok(result) {
            return;
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
        let rec = stick.recording();
        let target_ok = !target.trim().is_empty() && Path::new(&target).is_dir();
        let selected: u64 = prjs.iter().filter_map(|p| self.song_of(p)).map(|s| s.size).sum();
        let mut marks: HashMap<String, Mark> = HashMap::new();
        let mut play_marks: HashMap<String, Mark> = HashMap::new();
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
            for s in &plan.songs {
                marks.insert(s.plan.name.clone(), song_mark(s.action));
            }
            for p in &plan.playlists {
                play_marks.insert(p.name.clone(), playlist_mark(p.action));
            }
            let mut names: Vec<String> = fs::read_dir(&rec)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| e.path().is_dir())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .filter(|n| n != "#PLAYLISTS")
                        .collect()
                })
                .unwrap_or_default();
            for n in marks.keys() {
                if !names.contains(n) {
                    names.push(n.clone());
                }
            }
            names.sort_by_key(|n| n.to_lowercase());
            songs = names
                .into_iter()
                .map(|n| {
                    let m = marks.get(&n).copied().unwrap_or(Mark::None);
                    (n, m)
                })
                .collect();
            let mut pnames: Vec<String> = fs::read_dir(stick.playlists_dir())
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| p.extension().map(|x| x == "play").unwrap_or(false))
                        .map(|p| stem(&p.to_string_lossy()))
                        .collect()
                })
                .unwrap_or_default();
            for n in play_marks.keys() {
                if !pnames.contains(n) {
                    pnames.push(n.clone());
                }
            }
            pnames.sort_by_key(|n| n.to_lowercase());
            plays = pnames
                .into_iter()
                .map(|n| {
                    let m = play_marks.get(&n).copied().unwrap_or(Mark::None);
                    (n, m)
                })
                .collect();
        }
        self.preview = Some(Preview {
            key: k,
            target_ok,
            music: Path::new(&target).join("Music").is_dir(),
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

    fn start_export(&mut self, ctx: &egui::Context) {
        let target = self.config.stick_root.clone();
        if target.trim().is_empty() || !Path::new(&target).is_dir() {
            self.message("Export", "No target location selected.");
            return;
        }
        let (prjs, pls) = self.export_selection();
        if prjs.is_empty() && pls.is_empty() {
            self.message("Export", "Nothing selected.");
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
                            sync::Kind::Song => "Multitrack Song",
                            sync::Kind::Playlist => "Playlist",
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
                j.lines.push(("Export".to_string(), false, format!("ERROR: {}", e)));
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
            Ok(c) if c.is_empty() => self.message("Initialize Folders", "All folders already exist."),
            Ok(c) => {
                self.refresh();
                self.message("Initialize Folders", format!("Created:\n{}", c.join("\n")));
            }
            Err(e) => self.message("Initialize Folders", format!("Cannot create the folders:\n{}", e)),
        }
    }

    fn save_settings(&mut self) {
        self.config.apply_library();
        // other library: the old import scan does not fit any more
        self.import_scan = None;
        if let Err(e) = save_config(&config_path(), &self.config) {
            self.message("Settings", format!("Cannot save {}:\n{}", config_path().display(), e));
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
                Tab::Import => self.import(ui),
                Tab::EditTarget => self.edit_target(ui),
                Tab::Settings => self.settings(ui),
                Tab::Support => self.support(ui),
            });
        self.playlist_editor(ctx);
        self.playlist_update(ctx);
        self.song_editor(ctx, frame);
        self.export_overlay(ctx);
        self.dialogs(ctx);
        // rehearsal mix: saved at most once a second, bundled
        let now = std::time::Instant::now();
        if self.mix_saver.poll(now) {
            let _ = save_mixes(&config_path(), &self.config.mixes);
        }
        if let Some(wait) = self.mix_saver.wait(now) {
            ctx.request_repaint_after(wait);
        }
        // the position display moves while a song plays
        if self.rehearsal.is_playing() {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // the rehearsal mix is always saved when the program ends
        let _ = save_mixes(&config_path(), &self.config.mixes);
        self.rehearsal.shutdown();
    }
}

impl Lp16App {
    fn tab_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (t, title) in TABS {
                let (rect, resp) = ui.allocate_exact_size(vec2(78.0, 32.0), Sense::click());
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
                if panel_tab(ui, "Project Explorer", true) {
                    app.refresh();
                }
                panel_body(ui, true, |ui| app.home_tree(ui));
            },
            |app, ui| {
                panel_tab(ui, "Details", false);
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
                    let r = tree_row(ui, 1, Some(open), None, "", "Multitrack Songs", self.sel == Sel::SongsFolder, false, |p, rect| {
                        if songs_warn {
                            paint_warn(p, rect)
                        }
                    });
                    let mut new_song = false;
                    r.resp.context_menu(|ui| {
                        if ui.button("New Multitrack Song").clicked() {
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
                                if ui.button("Play in Rehearsal").clicked() {
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
                    let r = tree_row(ui, 1, Some(open), None, "", "Multitrack Playlists", self.sel == Sel::PlaylistsFolder, false, |p, rect| {
                        if pl_warn {
                            paint_warn(p, rect)
                        }
                    });
                    let mut new_pl = false;
                    r.resp.context_menu(|ui| {
                        if ui.button("New Multitrack Playlist").clicked() {
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
                            if r.double {
                                self.open_playlist_editor(Some(i));
                            } else if r.clicked {
                                self.sel = Sel::Playlist(i);
                            }
                        }
                    }
                    tree_row(ui, 1, Some(false), None, "", "Stereo Playlists", false, false, |_, _| {});
                }
                tree_row(ui, 0, Some(false), None, "", "Stereo Songs", false, false, |_, _| {});
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
                title(ui, "Multitrack Songs");
                let (mut new, mut refresh) = (false, false);
                ui.horizontal(|ui| {
                    new = small_btn(ui, "New Multitrack Song", true);
                    refresh = small_btn(ui, "Refresh", true);
                });
                ui.add_space(8.0);
                field_box(ui, "Target Disk Location", "\\Recording");
                ui.add_space(14.0);
                let mut srs: Vec<u32> = self.songs.iter().map(|s| s.sr).collect();
                srs.sort();
                srs.dedup();
                let mut bits: Vec<u32> = self.songs.iter().map(|s| s.bits).collect();
                bits.sort();
                bits.dedup();
                ui.label(format!("This branch contains {} Songs.", self.songs.len()));
                ui.label(format!(
                    "Sample rates in use: {}",
                    srs.iter().map(|&s| fmt_sr(s)).collect::<Vec<_>>().join(", ")
                ));
                ui.label(format!(
                    "Resolutions in use: {}",
                    bits.iter().map(|b| format!("{}-bit", b)).collect::<Vec<_>>().join(", ")
                ));
                if new {
                    self.new_song_dialog();
                }
                if refresh {
                    self.refresh();
                }
            }
            Sel::PlaylistsFolder => {
                title(ui, "Multitrack Playlists");
                let (mut new, mut refresh) = (false, false);
                ui.horizontal(|ui| {
                    new = small_btn(ui, "New Multitrack Playlist", true);
                    refresh = small_btn(ui, "Refresh", true);
                });
                ui.add_space(8.0);
                field_box(ui, "Target Disk Location", "\\Recording\\#PLAYLISTS");
                ui.add_space(14.0);
                ui.label(format!("This branch contains {} Playlists.", self.playlists.len()));
                if new {
                    self.open_playlist_editor(None);
                }
                if refresh {
                    self.refresh();
                }
            }
            Sel::Song(i) => {
                let mut act = 0;
                {
                    let s = &self.songs[i];
                    title(ui, &s.name);
                    ui.horizontal(|ui| {
                        small_btn(ui, "New MultiTrack Song", false);
                        if small_btn(ui, "Edit", true) {
                            act = 1;
                        }
                        small_btn(ui, "Mixer", false);
                        if small_btn(ui, "Rename", true) {
                            act = 2;
                        }
                        if small_btn(ui, "Delete", true) {
                            act = 3;
                        }
                        if small_btn(ui, "Duplicate", true) {
                            act = 4;
                        }
                    });
                    ui.add_space(8.0);
                    field_box(ui, "Target Disk Location", &format!("\\Recording\\{}", s.name));
                    ui.add_space(14.0);
                    kv(ui, "Type", "LP-16 multitrack Song");
                    kv(ui, "Sample rate", &fmt_sr(s.sr));
                    kv(ui, "Resolution", &format!("{}-bit", s.bits));
                    kv(ui, "Track count", &s.tracks.len().to_string());
                    ui.label("Tracks:");
                    list_box(ui, |ui| {
                        for t in &s.tracks {
                            ui.horizontal(|ui| {
                                ui.label(format!("{:>3}: {}", t.num, t.name));
                                if !t.exists {
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        badge(ui, "Missing Wav file")
                                            .on_hover_text(format!("Expected wav file:\n{}", t.wav));
                                    });
                                }
                            });
                        }
                    });
                    kv(ui, "Length", &fmt_len(s.duration));
                    kv(ui, "Size", &fmt_size(s.size));
                    kv(ui, "Modification Date", &fmt_date(s.modified));
                    ui.horizontal(|ui| {
                        ui.label(format!("MIDI file:  {}", if s.midi_name.is_empty() { "-" } else { &s.midi_name }));
                        if !s.midi_full.is_empty() && !s.midi_exists {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                badge(ui, "Missing MIDI file")
                                    .on_hover_text(format!("Expected midi file:\n{}", s.midi_full));
                            });
                        }
                    });
                    ui.label("Related Playlists:");
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
                    _ => {}
                }
            }
            Sel::Playlist(i) => {
                let mut act = 0;
                {
                    let pl = &self.playlists[i];
                    title(ui, &pl.name);
                    ui.horizontal(|ui| {
                        if small_btn(ui, "New Multitrack Playlist", true) {
                            act = 5;
                        }
                        if small_btn(ui, "Edit", true) {
                            act = 1;
                        }
                        if small_btn(ui, "Rename", true) {
                            act = 2;
                        }
                        if small_btn(ui, "Delete", true) {
                            act = 3;
                        }
                        if small_btn(ui, "Duplicate", true) {
                            act = 4;
                        }
                        if small_btn(ui, "Update Songs", true) {
                            act = 6;
                        }
                    });
                    ui.add_space(8.0);
                    field_box(ui, "Target Disk Location", "\\Recording\\#PLAYLISTS");
                    ui.add_space(14.0);
                    kv(ui, "Type", " multitrack Playlist");
                    kv(ui, "Song count", &pl.songs.len().to_string());
                    list_box(ui, |ui| {
                        for (n, s) in pl.songs.iter().enumerate() {
                            let info = self.song_of(&s.prj);
                            ui.horizontal(|ui| {
                                ui.label(format!("{:>3}: {}", n + 1, stem(&s.prj)));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    pause_widget(ui)
                                        .on_hover_text(&s.mode);
                                    match info {
                                        None => {
                                            badge(ui, "Missing Song").on_hover_text(&s.prj);
                                        }
                                        Some(x) if x.warn() => {
                                            badge(ui, "Missing files");
                                        }
                                        _ => {}
                                    }
                                });
                            });
                        }
                    });
                    kv(ui, "Length", &fmt_len(self.playlist_len(&pl.songs)));
                    kv(ui, "Size", &fmt_size(pl.size));
                    kv(ui, "Modification date", &fmt_date(pl.modified));
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
        ui.label(RichText::new("🗀").size(15.0)).on_hover_text("Target location");
        if ui
            .add(egui::Button::new(RichText::new("✏").size(13.0)).frame(false))
            .on_hover_text("Type a folder as target")
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
                .selected_text(if shown.is_empty() { "Select removable drive".to_string() } else { shown })
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
        panel_tab(ui, "Settings", false);
        panel_body(ui, false, |ui| {
            ui.horizontal(|ui| {
                ui.label("Target Type:");
                egui::ComboBox::from_id_source("target_type")
                    .width(ui.available_width() * 0.42)
                    .selected_text("LP-16")
                    .show_ui(ui, |ui| {
                        let _ = ui.selectable_label(true, "LP-16");
                    });
                ui.add_space(20.0);
                ui.label("Target:");
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
                ui.label("Selected:");
                ui.label(sel_t);
                ui.add_space(120.0);
                ui.label("To Copy:");
                ui.label(copy_t);
                ui.add_space(120.0);
                ui.label("Free:");
                ui.label(RichText::new(free_t).color(GREEN));
                ui.add_space(120.0);
                ui.label("Free After Export:");
                ui.label(RichText::new(after_t).color(GREEN));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    egui::ComboBox::from_id_source("mode")
                        .width(130.0)
                        .selected_text(if force { "Force Overwrite" } else { "Quick Export" })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut force, false, "Quick Export")
                                .on_hover_text("Only new and changed songs are written.");
                            ui.selectable_value(&mut force, true, "Force Overwrite")
                                .on_hover_text("Every selected song is written again.");
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
                if panel_tab(ui, "Project Explorer", true) {
                    app.refresh();
                }
                panel_body(ui, true, |ui| app.export_tree(ui));
            },
            |app, ui| {
                if panel_tab(ui, "Target Content", true) {
                    app.preview_gen += 1;
                    app.drives = list_drives();
                }
                panel_body(ui, true, |ui| app.target_content(ui));
            },
        );
        ui.add_space(6.0);
        let can_export = !self.x_songs.is_empty() || !self.x_playlists.is_empty();
        let mut go = false;
        ui.horizontal(|ui| {
            let w = ui.available_width();
            ui.add_space(w / 2.0 - 50.0);
            go = ui
                .add_enabled(
                    can_export,
                    egui::Button::new(RichText::new("Export").color(TEXT)).min_size(vec2(100.0, 28.0)),
                )
                .clicked();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled(false, egui::Button::new("Export Batch").min_size(vec2(110.0, 28.0)));
            });
        });
        if go {
            let ctx = ui.ctx().clone();
            self.start_export(&ctx);
        }
    }

    fn export_tree(&mut self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("Review missing information & warnings").size(11.0).color(DIM));
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
                let r = tree_row(ui, 0, Some(open), Some((c, true)), "", "Multitrack Songs", false, false, |p, rect| {
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
                            r.resp.on_hover_text("Exported with a checked playlist.");
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
                let r = tree_row(ui, 0, Some(open), Some((c, true)), "", "Multitrack Playlists", false, false, |p, rect| {
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
                tree_row(ui, 0, Some(false), Some((Check::Off, false)), "", "Stereo Playlists", false, false, |_, _| {});
                tree_row(ui, 0, Some(false), Some((Check::Off, false)), "", "Stereo Songs", false, false, |_, _| {});
            });
    }

    fn target_content(&mut self, ui: &mut Ui) {
        let p = self.preview.take().unwrap();
        if !p.target_ok {
            centered_text(ui, "No target location selected.");
        } else {
            egui::ScrollArea::vertical()
                .id_source("target_content")
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    if p.music {
                        tree_row(ui, 0, Some(false), None, "🗀", "Music", false, false, |_, _| {});
                    }
                    let open_rec = self.is_open("t:rec");
                    if tree_row(ui, 0, Some(open_rec), None, "🗀", "Recording", false, false, |_, _| {}).toggle {
                        self.toggle("t:rec");
                    }
                    if open_rec {
                        let open_pl = self.is_open("t:pl");
                        if tree_row(ui, 1, Some(open_pl), None, "🗀", "#PLAYLISTS", false, false, |_, _| {}).toggle {
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
                "Export",
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
                        format!("Exporting {}", current),
                        FontId::proportional(12.0),
                        Color32::WHITE,
                    );
                    ui.add_space(14.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "Copying to the target drive, it can take unpredictable time on USB Thumb Drive... {} remaining jobs.",
                                total - done
                            ))
                            .size(15.0),
                        );
                    });
                },
                &[("Cancel", true)],
            );
            if clicked.is_some() {
                job.lock().unwrap().cancel = true;
            }
            return;
        }
        let ok = j.lines.iter().all(|(_, ok, _)| *ok) && !j.cancel;
        let title = if j.cancel {
            "Export operation canceled."
        } else if ok {
            "Export operation successfully finished."
        } else {
            "Export finished with errors."
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
                ui.label(RichText::new(format!("Total time: {}", fmt_len(elapsed))).size(15.0));
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Note: Copy to external disk depends directly on the type of external disk unit. A USB Thumb Drive (Flash key) offers very poor performance compared to regular USB Hard Disk.")
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
            &[("OK", true)],
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
                let info = format!("{} Song(s) imported into the Project Library.", w.len());
                if !self.open_playlist_update(None, &w, &info) {
                    self.message("Import", info);
                }
            }
            Err(e) => self.message("Import", format!("The import stopped:\n{}\n\nSongs written before the error stay in the library.", e)),
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
            return Some("The Playlist Name is empty or contains invalid characters.".into());
        }
        if self.playlists.iter().any(|p| p.name.eq_ignore_ascii_case(name)) {
            return Some(format!("A Playlist \"{}\" already exists.", name));
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
            "Update Playlist to the newest Songs",
            vec2((screen.width() - 180.0).min(1300.0), screen.height()),
            |ui| self.playlist_update_body(ui, &mut up, problem.as_deref()),
            &[("Create Playlist", problem.is_none()), ("Cancel", true)],
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
            ui.label(RichText::new("Based on Playlist:").color(TEXT));
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
            ui.label(RichText::new("New Playlist Name:").color(TEXT));
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
        ui.label(RichText::new(format!("{} of {} Songs replaced. Start mode and order stay the same.", changed, pl.songs.len())).color(TEXT));
        ui.add_space(4.0);
        let lib = &up.lib;
        list_box(ui, |ui| {
            egui::ScrollArea::vertical().max_height(ui.available_height() - 8.0).auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("plu_rows").num_columns(4).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                    for h in ["#", "Current Song", "New Song", ""] {
                        ui.label(RichText::new(h).color(DIM));
                    }
                    ui.end_row();
                    for (n, s) in pl.songs.iter().enumerate() {
                        let cur = lib.iter().position(|x| key(&x.path) == key(&s.prj));
                        ui.label(RichText::new(format!("{}", n + 1)).color(DIM));
                        ui.label(RichText::new(stem(&s.prj)).color(TEXT));
                        let pick = &mut up.picks[n];
                        let shown = pick.map(|i| lib[i].name.clone()).unwrap_or_else(|| "(keep)".into());
                        egui::ComboBox::from_id_source(("plu_pick", n))
                            .width(380.0)
                            .selected_text(shown)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(pick, None, "(keep)");
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
                            Some(i) if up.fresh.contains(&key(&lib[i].path)) => ui.label(RichText::new("just imported").color(ORANGE)),
                            Some(_) => ui.label(RichText::new("newer version").color(ORANGE)),
                            None if cur.is_none() => ui.label(RichText::new("Missing Song").color(RED)),
                            None => ui.label(RichText::new("unchanged").color(DIM)),
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
            self.message("Update Playlist", format!("A Playlist \"{}\" already exists.", name));
            return;
        }
        let result = self.lib().create_playlist(&name, &to_pl_songs(&songs));
        if self.lib_ok(result) {
            self.refresh();
            self.select_playlist_name(&name);
            self.tab = Tab::Home;
            let n = up.picks.iter().flatten().count();
            self.message("Update Playlist", format!("Playlist \"{}\" created, {} Song(s) replaced.", name, n));
        }
    }

    fn import(&mut self, ui: &mut Ui) {
        panel_tab(ui, "Import Songs from the AudioLibrary", false);
        panel_body(ui, true, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("AudioLibrary:").color(TEXT));
                let lib = if self.config.import_lib.is_empty() { "(not set, see Settings)" } else { &self.config.import_lib };
                ui.label(RichText::new(lib).color(DIM));
                ui.add_space(12.0);
                if small_btn(ui, "Import Songs", !self.config.import_lib.is_empty()) {
                    self.import_rescan();
                }
                if small_btn(ui, "Update Playlist…", !self.playlists.is_empty())
                    && !self.open_playlist_update(None, &[], "")
                {
                    self.message("Update Playlist", "All Playlists already use the newest version of every Song.");
                }
            });
            ui.label(
                RichText::new("Layout: <AudioLibrary>\\<Song>\\<Date + note>\\*.wav + 1 .mid.  New date folders (newer than the newest version in the library) get a Song named <Song>_(<Date + note>). Folders without a date are skipped.")
                    .size(11.0)
                    .color(DIM),
            );
            ui.add_space(6.0);
            let items = match &self.import_scan {
                None => {
                    centered_text(ui, "Click \"Import Songs\" to scan the AudioLibrary.");
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
                ui.label(RichText::new(format!(
                    "{} date folders:  {} new,  {} with problems,  {} in the library,  {} old without Song",
                    items.len(), new, problems, present, old
                )).color(TEXT));
                ui.add_space(16.0);
                ui.checkbox(&mut self.import_show_all, "Show all (also in library and old folders)");
            });
            ui.add_space(4.0);
            let selected = self.import_selected().len();
            let list_h = (ui.available_height() - 40.0).max(80.0);
            list_box(ui, |ui| {
                egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
                    egui::Grid::new("import").num_columns(5).striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
                        for h in ["", "Song (library name)", "Tracks", "MIDI", "Status"] {
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
                            let fmt = if it.rate > 0 { format!("{} ch - {} - {}-bit", it.wavs.len(), fmt_sr(it.rate), it.bits) } else { format!("{} ch", it.wavs.len()) };
                            ui.label(RichText::new(format!("{}   {}", fmt, fmt_len(it.duration))).color(TEXT));
                            let midi = it.midi.as_deref().map(file_name).unwrap_or_else(|| "-".into());
                            ui.label(RichText::new(midi).color(TEXT));
                            let notes = if it.notes.is_empty() { String::new() } else { format!(" ({})", it.notes.join(", ")) };
                            let (text, color) = match (&it.status, &it.older_than) {
                                (ImportStatus::Missing, None) => (format!("new{}", notes), ORANGE),
                                (ImportStatus::Missing, Some(n)) => (format!("old folder, newer version: {}{}", n, notes), DIM),
                                (ImportStatus::Present(n), _) => (format!("in library: {}", n), GREEN),
                                (ImportStatus::Duplicate(n), _) => (format!("in library: {}", n.join(", ")), GREEN),
                                (ImportStatus::Invalid(e), None) => (format!("cannot import: {}", e), RED),
                                (ImportStatus::Invalid(e), Some(_)) => (format!("old folder, cannot import: {}", e), DIM),
                            };
                            ui.label(RichText::new(text).color(color));
                            ui.end_row();
                        }
                        if shown == 0 {
                            ui.label("");
                            ui.label(RichText::new("No new date folders: every Song is up to date.").color(GREEN));
                            ui.end_row();
                        }
                    });
                });
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if small_btn(ui, "Select new", new > 0) {
                    for (c, it) in self.import_check.iter_mut().zip(&items) {
                        *c = it.is_new() || (*c && it.status == ImportStatus::Missing);
                    }
                }
                if small_btn(ui, "Select none", selected > 0) {
                    self.import_check.iter_mut().for_each(|c| *c = false);
                }
                ui.add_space(20.0);
                if small_btn(ui, &format!("Import {} selected Song(s)", selected), selected > 0) {
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
                    ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).size(22.0));
                    ui.add_space(10.0);
                    ui.label("A rebuild of the uTool2 workflow for the Cymatic Audio LP-16:");
                    ui.label("project library, playlists and export to the stick.");
                    ui.add_space(10.0);
                    ui.label(RichText::new(format!("Settings file: {}", config_path().display())).color(DIM));
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
                panel_tab(ui, "Target Explorer", false);
                panel_body(ui, true, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Target location:");
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
                    app.target_tree(ui);
                });
            },
            |app, ui| {
                panel_tab(ui, "Details", false);
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
            centered_text(ui, "No target location selected.");
            return;
        }
        let rec = root.join("Recording");
        let list = |dir: &Path, dirs: bool, ext: &str| -> Vec<String> {
            let mut v: Vec<String> = fs::read_dir(dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| {
                            if dirs {
                                p.is_dir()
                            } else {
                                p.extension().map(|x| x == ext).unwrap_or(false)
                            }
                        })
                        .map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
                        .filter(|n| n != "#PLAYLISTS")
                        .collect()
                })
                .unwrap_or_default();
            v.sort_by_key(|n| n.to_lowercase());
            v
        };
        let mut clicked: Option<EtSel> = None;
        let mut find: Option<String> = None;
        egui::ScrollArea::vertical()
            .id_source("target_tree")
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (folder, k) in [("Music", "e:music"), ("Recording", "e:rec")] {
                    let base = root.join(folder);
                    if !base.is_dir() {
                        continue;
                    }
                    let open = self.is_open(k);
                    if tree_row(ui, 0, Some(open), None, "🗀", folder, false, false, |_, _| {}).toggle {
                        self.toggle(k);
                    }
                    if !open {
                        continue;
                    }
                    let pk = format!("{}:pl", k);
                    let pl_dir = base.join("#PLAYLISTS");
                    if pl_dir.is_dir() {
                        let open_pl = self.is_open(&pk);
                        if tree_row(ui, 1, Some(open_pl), None, "🗀", "#PLAYLISTS", false, false, |_, _| {}).toggle {
                            self.toggle(&pk);
                        }
                        if open_pl && base == rec {
                            for n in list(&pl_dir, false, "play") {
                                let s = stem(&n);
                                let sel = self.et_sel == Some(EtSel::Play(s.clone()));
                                if tree_row(ui, 2, None, None, "☰", &s, sel, true, |_, _| {}).clicked {
                                    clicked = Some(EtSel::Play(s));
                                }
                            }
                        }
                    }
                    if base == rec {
                        for n in list(&rec, true, "") {
                            let sel = self.et_sel == Some(EtSel::Song(n.clone()));
                            let r = tree_row(ui, 1, None, None, "♫", &n, sel, true, |_, _| {});
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
        if let Some(n) = find {
            if let Some(i) = self.songs.iter().position(|s| s.name == n) {
                self.sel = Sel::Song(i);
                self.open.insert("h:root".into());
                self.open.insert("h:songs".into());
                self.tab = Tab::Home;
            }
        }
    }

    fn compute_et_details(&self, sel: &EtSel) -> EtDetails {
        let rec = Path::new(&self.config.stick_root).join("Recording");
        let pl_dir = rec.join("#PLAYLISTS");
        match sel {
            EtSel::Song(name) => {
                let dir = rec.join(name);
                let set = fs::read_to_string(dir.join("song.set")).unwrap_or_default();
                let sr: u32 = plain_tag(&set, "sampling_rate").and_then(|s| s.parse().ok()).unwrap_or(0);
                let bits: u32 = plain_tag(&set, "resolution").and_then(|s| s.parse().ok()).unwrap_or(0);
                let midi_len: f64 = plain_tag(&set, "midi_length").and_then(|s| s.parse().ok()).unwrap_or(0.0);
                // length from the largest WAV payload
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
                let wav_len = if sr > 0 && bits > 0 {
                    max_wav.saturating_sub(STICK_WAV_HEADER_LEN) as f64 / (sr as f64 * bits as f64 / 8.0)
                } else {
                    0.0
                };
                let related: Vec<String> = fs::read_dir(&pl_dir)
                    .map(|rd| {
                        rd.filter_map(|e| e.ok())
                            .map(|e| e.path())
                            .filter(|p| p.extension().map(|x| x == "play").unwrap_or(false))
                            .filter(|p| {
                                fs::read_to_string(p)
                                    .map(|t| t.contains(&format!("<name>{}</name>", name)))
                                    .unwrap_or(false)
                            })
                            .map(|p| stem(&p.to_string_lossy()))
                            .collect()
                    })
                    .unwrap_or_default();
                EtDetails {
                    title: name.clone(),
                    lines: vec![
                        ("Type".into(), "LP-16 Multitrack Song".into()),
                        ("Sample rate".into(), fmt_sr(sr)),
                        ("Resolution".into(), format!("{}-bit", bits)),
                        ("Channel count".into(), plain_tag(&set, "channel_count").unwrap_or_default()),
                        ("Length".into(), fmt_len(wav_len.max(midi_len))),
                        ("Size".into(), fmt_size(dir_size(&dir))),
                        ("Modification date".into(), fmt_date(mtime(&dir.join("song.set")))),
                        ("MIDI file".into(), plain_tag(&set, "midi_file").unwrap_or_default()),
                    ],
                    list_title: "Related Playlists:".into(),
                    list: related,
                    play_songs: false,
                }
            }
            EtSel::Play(name) => {
                let path = pl_dir.join(format!("{}.play", name));
                let text = fs::read_to_string(&path).unwrap_or_default();
                let names = xml_all(&text, "name");
                let len: f64 = xml_all(&text, "duration").iter().filter_map(|d| d.parse::<f64>().ok()).sum();
                let size: u64 = names.iter().map(|n| dir_size(&rec.join(n))).sum();
                EtDetails {
                    title: name.clone(),
                    lines: vec![
                        ("Type".into(), "LP-16 multitrackplaylist".into()),
                        ("Song count".into(), names.len().to_string()),
                    ],
                    list_title: String::new(),
                    list: names
                        .iter()
                        .enumerate()
                        .map(|(i, n)| format!("{:>3}: {}", i + 1, n))
                        .chain(std::iter::once(String::new()))
                        .chain(
                            [
                                format!("Length:  {}", fmt_len(len)),
                                format!("Size:  {}", fmt_size(size)),
                                format!("Modification date:  {}", fmt_date(mtime(&path))),
                            ]
                            .into_iter(),
                        )
                        .collect(),
                    play_songs: true,
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
        ui.horizontal(|ui| {
            small_btn(ui, "Delete", false);
            if d.play_songs {
                small_btn(ui, "New Playlist", false);
                small_btn(ui, "Edit", false);
            } else {
                small_btn(ui, "Mixer", false);
            }
        });
        ui.add_space(8.0);
        for (k, v) in &d.lines {
            kv(ui, k, v);
        }
        if d.play_songs {
            let n = d.list.iter().position(|l| l.is_empty()).unwrap_or(d.list.len());
            list_box(ui, |ui| {
                for l in &d.list[..n] {
                    ui.horizontal(|ui| {
                        ui.label(l);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| pause_widget(ui));
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
            ui.label(RichText::new("Double-click on a Song to find it in the Project Explorer").size(11.5).color(DIM));
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
                panel_tab(ui, "Devices", false);
                panel_body(ui, false, |ui| {
                    ui.label("Select the devices to be managed by this application.");
                    for (name, on) in [("uTrack-X32", false), ("uTrack24", false), ("LP-16", true), ("LR-16", false)] {
                        let mut v = on;
                        ui.add_enabled(false, egui::Checkbox::new(&mut v, name));
                    }
                });
                ui.add_space(12.0);
                panel_tab(ui, "Default File Location", false);
                let mut save = false;
                let mut init = false;
                panel_body(ui, false, |ui| {
                    egui::Grid::new("paths").num_columns(3).spacing([8.0, 6.0]).show(ui, |ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label("Library Folder:"));
                        if folder_btn(ui, "Choose folder") {
                            let mut dlg = rfd::FileDialog::new().set_title("Library Folder");
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
                        for (label, path) in [("Project Library:", derived.project_lib), ("Audio Library:", derived.audio_lib)] {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(RichText::new(label).color(DIM)));
                            ui.label("");
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(if path.is_empty() { "-" } else { &path }).color(DIM));
                                if !path.is_empty() && !Path::new(&path).is_dir() {
                                    ui.label(RichText::new("(missing, use Initialize Folders)").color(ORANGE));
                                }
                            });
                            ui.end_row();
                        }
                    });
                    ui.label(
                        RichText::new(format!(
                            "Below the Library Folder: {} = songs and playlists, {} = <Song>\\<Date + note>\\*.wav + 1 .mid (audio files and import). Saved when you leave the field (lp16-tool.toml).",
                            lp16_tool::PROJECT_LIB_NAME,
                            lp16_tool::AUDIO_LIB_NAME
                        ))
                        .size(11.0)
                        .color(DIM),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if small_btn(ui, "Initialize Folders", !self.config.library.trim().is_empty()) {
                            init = true;
                        }
                        ui.label(
                            RichText::new("Creates the missing folders: CymaticProjectLib (with LP16\\Multitrack Songs, …) and AudioLibrary.")
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
            });
        });
    }

    /// The "Audio output" area of the Settings tab (gui_audio.rs): changes
    /// are saved at once and handed to the rehearsal player, which uses
    /// them the next time it opens its stream.
    fn audio_output(&mut self, ui: &mut Ui) {
        let running = self.rehearsal.stream_info().cloned();
        let playing = self.rehearsal.is_playing();
        let error = self.rehearsal.error().map(str::to_string);
        let out = self.audio.ui(ui, &mut self.config, running.as_ref(), playing, error.as_deref());
        if out.changed {
            self.rehearsal.set_settings(AudioSettings::from(&self.config));
            if let Err(e) = save_config(&config_path(), &self.config) {
                self.message("Settings", format!("Cannot save {}:\n{}", config_path().display(), e));
            }
        }
        if out.tone {
            // the tone may need the device that the idle stream holds
            self.rehearsal.park();
            self.audio.start_tone(&self.config);
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
            "Edit Multitrack Playlist",
            vec2(screen.width() - 180.0, screen.height()),
            |ui| self.playlist_editor_body(ui, &mut ed),
            &[("Save", can_save), ("Close Editor", true)],
        );
        match ed.drag {
            Some(PlDrag::Rows) => {
                let sel: Vec<usize> = (0..ed.songs.len()).filter(|&k| ed.checked[k]).collect();
                let text = if sel.len() == 1 { stem(&ed.songs[sel[0]].prj) } else { format!("{} songs", sel.len()) };
                paint_drag_ghost(ctx, text);
            }
            Some(PlDrag::Song(i)) => {
                if let Some(s) = self.songs.get(i) {
                    paint_drag_ghost(ctx, s.name.clone());
                }
            }
            None => {}
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
        // Playlist Settings
        panel_tab(ui, "Playlist Settings", false);
        panel_body(ui, false, |ui| {
            ui.horizontal(|ui| {
                ui.label("     Location:");
                ui.label("LP16\\Multitrack Playlists");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(140.0);
                    ui.label(format!("Song Count: {}", ed.songs.len()));
                });
            });
            ui.horizontal(|ui| {
                ui.label("Playlist Name:");
                let w = ui.available_width() - 360.0;
                if ui.add(egui::TextEdit::singleline(&mut ed.name).desired_width(w)).changed() {
                    ed.dirty = true;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(100.0);
                    ui.label(format!("Playlist Length: {}", fmt_len(self.playlist_len(&ed.songs))));
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
                panel_tab(ui, "Available Songs", false);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if small_btn(ui, "Add ▶", ed.avail_sel.is_some()) {
                        add = ed.avail_sel;
                    }
                });
            });
            panel_body(ui, true, |ui| {
                egui::ScrollArea::vertical().id_source("avail").auto_shrink([false; 2]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let r = tree_row(ui, 0, Some(ed.avail_open), None, "", "Multitrack Songs", false, false, |_, _| {});
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
        // gap (0..=len) under the mouse while something is dragged over the content
        let mut gap: Option<usize> = None;
        let mut move_by: i32 = 0;
        ui.allocate_ui_at_rect(Rect::from_min_size(top + vec2(half + 30.0, 0.0), vec2(half, h)), |ui| {
            panel_tab(ui, "Playlist Content", false);
            panel_body(ui, true, |ui| {
                let any = ed.checked.iter().any(|&c| c);
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        delete = small_btn(ui, "Delete", any);
                        if small_btn(ui, "Down", any) {
                            move_by = 1;
                        }
                        if small_btn(ui, "Up", any) {
                            move_by = -1;
                        }
                    });
                });
                // header
                let (hr, hresp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                let p = ui.painter_at(hr);
                let all = !ed.checked.is_empty() && ed.checked.iter().all(|&c| c);
                let state = if all { Check::On } else if any { Check::Partial } else { Check::Off };
                paint_check(&p, Rect::from_center_size(pos2(hr.left() + 12.0, hr.center().y), vec2(14.0, 14.0)), state, true);
                let f = FontId::proportional(11.0);
                p.text(pos2(hr.left() + 70.0, hr.center().y), Align2::CENTER_CENTER, "Start Mode", f.clone(), TEXT);
                p.text(pos2((hr.left() + hr.right() - 170.0) / 2.0, hr.center().y), Align2::CENTER_CENTER, "Song Name", f.clone(), TEXT);
                p.text(pos2(hr.right() - 120.0, hr.center().y), Align2::CENTER_CENTER, "Properties", f.clone(), TEXT);
                p.text(pos2(hr.right() - 40.0, hr.center().y), Align2::CENTER_CENTER, "Length", f, TEXT);
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
                    for k in 0..ed.songs.len() {
                        let s = &ed.songs[k];
                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click_and_drag());
                        if let Some(q) = over {
                            if gap.is_none() && q.y < rect.center().y {
                                gap = Some(k);
                                gap_y = Some(rect.top());
                            }
                        }
                        let p = ui.painter_at(rect);
                        if ed.checked[k] {
                            p.rect_filled(rect, 0.0, SEL);
                        } else if resp.hovered() {
                            p.rect_filled(rect, 0.0, HOVER);
                        }
                        let cy = rect.center().y;
                        paint_check(&p, Rect::from_center_size(pos2(rect.left() + 12.0, cy), vec2(14.0, 14.0)), if ed.checked[k] { Check::On } else { Check::Off }, true);
                        p.text(pos2(rect.left() + 22.0, cy), Align2::LEFT_CENTER, (k + 1).to_string(), FontId::proportional(13.0), TEXT);
                        paint_mode(&p, pos2(rect.left() + 70.0, cy), &s.mode);
                        p.text(pos2(rect.left() + 102.0, cy), Align2::LEFT_CENTER, stem(&s.prj), FontId::proportional(13.5), TEXT);
                        match self.song_of(&s.prj) {
                            Some(info) => paint_song_cols(&p, rect, info),
                            None => {
                                p.text(pos2(rect.right() - 120.0, cy), Align2::CENTER_CENTER, "missing", FontId::proportional(11.0), ORANGE);
                                paint_warn(&p, rect);
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
                        gap = Some(ed.songs.len());
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
                    let before: Vec<String> = ed.songs.iter().map(|s| s.prj.clone()).collect();
                    ed.checked = move_checked(&mut ed.songs, &ed.checked, g);
                    ed.anchor = ed.checked.iter().position(|&c| c);
                    if ed.songs.iter().map(|s| &s.prj).ne(before.iter()) {
                        ed.dirty = true;
                    }
                }
                (Some(PlDrag::Song(i)), Some(g)) => {
                    ed.songs.insert(
                        g,
                        PlSong {
                            prj: self.songs[i].path.clone(),
                            mode: "wait_for_button_press".into(),
                            delay: "0".into(),
                        },
                    );
                    ed.checked = (0..ed.songs.len()).map(|k| k == g).collect();
                    ed.anchor = Some(g);
                    ed.dirty = true;
                }
                _ => {}
            }
        }
        if let Some(i) = add {
            ed.songs.push(PlSong {
                prj: self.songs[i].path.clone(),
                mode: "wait_for_button_press".into(),
                delay: "0".into(),
            });
            ed.checked.push(false);
            ed.dirty = true;
        }
        if delete {
            let mut k = 0;
            ed.songs.retain(|_| {
                let keep = !ed.checked[k];
                k += 1;
                keep
            });
            ed.checked = vec![false; ed.songs.len()];
            ed.anchor = None;
            ed.dirty = true;
        }
        if move_by != 0 {
            let n = ed.songs.len();
            let order: Vec<usize> = if move_by < 0 { (1..n).collect() } else { (0..n.saturating_sub(1)).rev().collect() };
            for i in order {
                let j = (i as i32 + move_by) as usize;
                if ed.checked[i] && !ed.checked[j] {
                    ed.songs.swap(i, j);
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
                self.rehearsal.play_song(&prj);
                self.tab = Tab::Rehearsal;
            }
            Err(e) => self.message("Play in Rehearsal", format!("The Song can't be opened:
{}", e)),
        }
    }

    fn open_song_editor(&mut self, i: usize) {
        if let Some(s) = self.songs.get(i) {
            match SongEditor::load(&s.path, &self.config.audio_lib) {
                Ok(ed) => self.song_ed = Some(ed),
                Err(e) => self.message("Edit Multitrack Song", format!("The Song can't be opened:\n{}", e)),
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
            "Edit Multitrack Song",
            vec2(screen.width() - 180.0, screen.height()),
            |ui| song_editor_body(ui, &mut ed, &audio_lib, frame),
            &[("Save", can_save), ("Close Editor", true)],
        );
        // drag ghost next to the mouse pointer
        if let Some(files) = &ed.drag {
            let text = if files.len() == 1 {
                Path::new(&files[0]).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            } else {
                format!("{} files", files.len())
            };
            paint_drag_ghost(ctx, text);
        }
        if let Some(text) = ed.message.take() {
            self.message("Edit Multitrack Song", text);
        }
        let path = ed.path.clone();
        self.song_ed = Some(ed);
        match clicked {
            Some(0) => {
                self.rehearsal.song_changed(&path);
                let ed = self.song_ed.as_mut().unwrap();
                match ed.save() {
                    Ok(()) => {
                        self.refresh();
                        self.select_song_path(Path::new(&path));
                    }
                    Err(e) => self.message("Edit Multitrack Song", format!("The Song could not be saved:\n{}", e)),
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
                if overlay(ctx, "dlg", order, title, size, |ui| centered_text(ui, text), &[("OK", true)]).is_some() {
                    close = true;
                }
            }
            Dialog::SongName { .. } | Dialog::PlaylistName { .. } | Dialog::NewSong { .. } => {}
            Dialog::ImportConfirm => {
                let names: Vec<String> = self.import_selected().iter().map(|i| i.name.clone()).collect();
                let mut list = names.iter().take(12).cloned().collect::<Vec<_>>().join("\n");
                if names.len() > 12 {
                    list += &format!("\n… and {} more", names.len() - 12);
                }
                match overlay(
                    ctx,
                    "dlg",
                    order,
                    "Import Songs",
                    vec2(920.0, 420.0),
                    |ui| centered_text(ui, &format!("Create these {} Song(s) in the Project Library?\n\n{}", names.len(), list)),
                    &[("Yes, import", true), ("No, cancel", true)],
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
                    "Delete Song",
                    size,
                    |ui| centered_text(ui, &format!("Do you want to delete the Song \"{}\"?\n(only the song project in the library, the audio files stay)", name)),
                    &[("Yes, delete the Song", true), ("No, cancel", true)],
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
                    "Delete Playlist",
                    size,
                    |ui| centered_text(ui, &format!("Do you want to delete the Playlist \"{}\"?", name)),
                    &[("Yes, delete the Playlist", true), ("No, cancel", true)],
                ) {
                    Some(0) => {
                        close = true;
                        self.delete_playlist(i);
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
                    "Playlist Name Change",
                    size,
                    |ui| centered_text(ui, "Do you want to rename the Playlist file or to create a copy?"),
                    &[("Rename", true), ("Copy", true), ("Cancel", true)],
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
                    "Edit Multitrack Playlist",
                    size,
                    |ui| centered_text(ui, "The Playlist has unsaved changes."),
                    &[("Yes, close without saving", true), ("No, cancel", true)],
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
                    "Edit Multitrack Song",
                    size,
                    |ui| centered_text(ui, "The Song has unsaved changes."),
                    &[("Yes, close without saving", true), ("No, cancel", true)],
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
        let mut name = "New Song".to_string();
        let mut n = 2;
        while dir.join(format!("{}.songprj", name)).exists() {
            name = format!("New Song {}", n);
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
            "New Multitrack Song",
            vec2(920.0, 340.0),
            |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    ui.label(RichText::new("Name of the new Song:").size(15.0));
                    ui.add_space(14.0);
                    let r = ui.add(egui::TextEdit::singleline(name).desired_width(600.0));
                    r.request_focus();
                    ui.add_space(8.0);
                    if invalid {
                        ui.label(RichText::new("The Song Name contains invalid characters.").color(ORANGE));
                    } else if exists {
                        ui.label(RichText::new("A Song with this name already exists.").color(ORANGE));
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.add_space((ui.available_width() - 420.0).max(0.0) / 2.0);
                        ui.label("Sample rate:");
                        ui.radio_value(sr, 44100, "44.1 kHz");
                        ui.radio_value(sr, 48000, "48 kHz");
                        ui.add_space(24.0);
                        ui.label("Resolution:");
                        ui.radio_value(bits, 16, "16-bit");
                        ui.radio_value(bits, 24, "24-bit");
                    });
                });
            },
            &[("Create", ok), ("Cancel", true)],
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
        let what = if is_song { "Song" } else { "Playlist" };
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
            format!("Name of the copy of the {} \"{}\":", what, current)
        } else {
            format!("Do you want to rename the {} file or to create a copy?", what)
        };
        let buttons: Vec<(&str, bool)> = if copy_only {
            vec![("Copy", ok), ("Cancel", true)]
        } else {
            vec![("Rename", ok), ("Copy", ok), ("Cancel", true)]
        };
        let clicked = overlay(
            ctx,
            "dlg",
            egui::Order::Foreground,
            &format!("{} Name Change", what),
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
                        ui.label(RichText::new(format!("The new {} Name contains invalid characters.", what)).color(ORANGE));
                    } else if exists && trimmed != current {
                        ui.label(RichText::new(format!("A {} with this name already exists.", what)).color(ORANGE));
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

/// start mode symbol: uTool2 shows a pause sign for "wait for button press"
fn paint_mode(p: &egui::Painter, c: egui::Pos2, mode: &str) {
    if mode == "wait_for_button_press" || mode.is_empty() {
        for dx in [-3.5f32, 3.5] {
            p.rect_filled(Rect::from_center_size(pos2(c.x + dx, c.y), vec2(4.5, 14.0)), 0.0, TEXT);
        }
    } else {
        p.add(egui::Shape::convex_polygon(
            vec![pos2(c.x - 5.0, c.y - 7.0), pos2(c.x + 6.0, c.y), pos2(c.x - 5.0, c.y + 7.0)],
            TEXT,
            Stroke::NONE,
        ));
    }
}

fn pause_widget(ui: &mut Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
    paint_mode(ui.painter(), rect.center(), "wait_for_button_press");
    resp.on_hover_text("Start mode: wait for button press")
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
            } else if name.to_lowercase().ends_with(".wav") {
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
        };
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
                Ok(i) => rejected.push(format!(
                    "{}  ({} ch, {}, {}-bit)",
                    file_name(f),
                    i.channels,
                    fmt_sr(i.rate),
                    i.bits
                )),
                Err(_) => rejected.push(format!("{}  (not a usable WAV file)", file_name(f))),
            }
        }
        self.recalc(audio_lib);
        let mut msg = String::new();
        if !rejected.is_empty() {
            msg += &format!(
                "The Song needs mono WAV files with {} / {}-bit.\nNot added:\n{}",
                fmt_sr(self.sr),
                self.bits,
                rejected.join("\n")
            );
        }
        if overflow > 0 {
            if !msg.is_empty() {
                msg += "\n\n";
            }
            msg += &format!("{} file(s) did not fit: the LP-16 has 16 tracks.", overflow);
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
            self.message = Some(format!("{}\nis not a readable MIDI file.", file_name(full)));
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
    fn save(&mut self) -> Result<(), String> {
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
        prj.save().map_err(|e| e.to_string())?;
        self.prj = prj;
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
fn folder_btn(ui: &mut Ui, tip: &str) -> bool {
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
    panel_tab(ui, "Multitrack Song Settings", false);
    panel_body(ui, false, |ui| {
        ui.horizontal(|ui| {
            ui.label("       Location:");
            ui.label("LP16\\Multitrack Songs");
        });
        ui.horizontal(|ui| {
            ui.label("   Song Name:");
            let mut n = ed.name.clone();
            ui.add_enabled(false, egui::TextEdit::singleline(&mut n).desired_width(ui.available_width() - 20.0));
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("Synched Units:").color(DIM));
            ui.label(RichText::new("Standalone (1)").color(DIM));
            ui.add_space(200.0);
            ui.label("Sampling Rate:");
            ui.label(fmt_sr(ed.sr));
            ui.add_space(200.0);
            ui.label("Resolution:");
            ui.label(format!("{}-bit", ed.bits));
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
        panel_tab(ui, "Audio Files", false);
        panel_body(ui, true, |ui| {
            ui.horizontal(|ui| {
                if folder_btn(ui, "Open the folder with the WAV files") {
                    let start = if Path::new(&ed.folder).is_dir() { ed.folder.clone() } else { audio_lib.to_string() };
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_title("Audio Files")
                        .set_directory(&start)
                        .set_parent(frame)
                        .pick_folder()
                    {
                        ed.open_folder(dir.to_string_lossy().into_owned());
                    }
                }
                if icon_btn(ui, "🔄", "Reload", true) {
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
                p.text(pos2(hr.center().x, hr.center().y), Align2::CENTER_CENTER, "File Name", FontId::proportional(12.0), DIM);
                p.text(pos2(hr.right() - 30.0, hr.center().y), Align2::CENTER_CENTER, "Length", FontId::proportional(12.0), DIM);
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
                    if tree_row(ui, 0, None, None, " ", "[Up]", false, false, |p, r| paint_folder(p, pos2(r.left() + 33.0, r.center().y), TEXT)).double {
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
                            Some(inf) => format!(
                                "{} ch, {}, {}-bit - the Song needs mono, {}, {}-bit",
                                inf.channels,
                                fmt_sr(inf.rate),
                                inf.bits,
                                fmt_sr(ed.sr),
                                ed.bits
                            ),
                            None => "not a usable WAV file".into(),
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
        panel_tab(ui, "Tracks", false);
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
                    ui.add_enabled(false, egui::Button::new("Mixer").frame(false));
                    ui.add_space(20.0);
                    delete = ui
                        .add_enabled(n_checked > 0, egui::Button::new("Delete").frame(false))
                        .on_hover_text("Remove the checked tracks from the Song")
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
                    let resp = resp.on_hover_text(if missing { format!("Missing Wav file:\n{}", full) } else { full });
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
        panel_tab(ui, "MIDI File", false);
        panel_body(ui, true, |ui| {
            let full = if ed.midi.is_empty() { String::new() } else { resolve(&ed.midi, audio_lib) };
            ui.horizontal(|ui| {
                if folder_btn(ui, "Choose the MIDI file") {
                    let start = Path::new(&full)
                        .parent()
                        .filter(|d| d.is_dir())
                        .map(|d| d.to_string_lossy().into_owned())
                        .unwrap_or_else(|| ed.folder.clone());
                    if let Some(f) = rfd::FileDialog::new()
                        .set_title("MIDI File")
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
                    if icon_btn(ui, "🗑", "Remove the MIDI file from the Song", !ed.midi.is_empty()) {
                        ed.midi.clear();
                        ed.dirty = true;
                        ed.recalc(audio_lib);
                    }
                    path_field(ui, if full.is_empty() { "-" } else { &full }, if missing { ORANGE } else { TEXT });
                });
            });
            let len = ed.wav_len().max(ed.midi_len.unwrap_or(0.0));
            ui.horizontal(|ui| {
                ui.label(format!("Duration: {}", fmt_len(len)));
                if !full.is_empty() && ed.midi_len.is_none() {
                    badge(ui, "Missing MIDI file");
                }
            });
        });
    });
    ui.allocate_rect(Rect::from_min_size(top, vec2(w, h)), Sense::hover());
}

/// Run the GUI. Settings are written to lp16-tool.toml.
pub fn run_gui(config: Config) {
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

//! The mixer below the player in the Rehearsal tab, in the style of the
//! uTool2 mixer: narrow, tall channel strips (16 strips and the master side
//! by side) and a header bar. The strips fill the width and height they are
//! given: they get wider and the faders taller with the frame, down to a
//! minimum (then the area scrolls).
//!
//! It edits either the global rehearsal mix (the preset bar is the header)
//! or, in song mode, the song mix of the loaded song: the uTool2 mixer
//! values in its .songprj (`lp16_tool::mix::SongStrip`), which the
//! rehearsal adds to the rehearsal mix. All the logic is in the library; the
//! helpers here are only about drawing (fader taper, scale marks, knob
//! angle, strip sizes).

use crate::gui_app::{small_btn, DIM, LINE, ORANGE, PANEL, RED, TEXT};
use crate::gui_mixer::{double_clicked, fmt_pan, meter_sized, toggle, update_shown, MixerOutcome, PresetBar, MUTE_ON, SOLO_ON};
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Margin, Rect, RichText, Sense, Stroke, Ui};
use lp16_tool::backup_mix::BackupSide;
use lp16_tool::mix::{parse_level, MixLibrary, SongStrip, LEVEL_MAX_DB, LEVEL_MIN_DB, PAN_MAX, STRIPS};
use lp16_tool::rehearsal::Levels;

/// Outer width of a channel strip (frame margins included) and of the
/// master at the normal size; the strips scale from these.
const STRIP_W: f32 = 92.0;
const MASTER_W: f32 = 116.0;
const MARGIN: f32 = 4.0;
/// The narrowest and the widest strip; below the narrowest the area scrolls
/// sideways.
const MIN_STRIP_W: f32 = 70.0;
const MAX_STRIP_W: f32 = 180.0;
/// Space between two strips, and the extra space before the master.
const GAP: f32 = 4.0;
const MASTER_GAP: f32 = 8.0;
/// Width of the dB scale, the fader and one meter at the normal size.
const SCALE_W: f32 = 30.0;
const MIN_SCALE_W: f32 = 26.0;
const FADER_W: f32 = 22.0;
const METER_W: f32 = 10.0;
/// Height of the fader handle; the track is inset by half of it.
const HANDLE_H: f32 = 14.0;
/// Size of the pan knob at the normal size, and its limits.
const KNOB: f32 = 40.0;
const MIN_KNOB: f32 = 32.0;
const MAX_KNOB: f32 = 56.0;
/// The S/M buttons and the space below them; the top part of a strip is
/// this and the knob.
const BUTTONS_H: f32 = 23.0;
/// What the rest of a strip needs besides the fader (texts, spacing,
/// frame, the scroll bar) with the normal knob, and the lowest fader drawn.
const OVERHEAD: f32 = 170.0 + HINT_H + 3.0;
/// Height of the line below the track name (the song's offset).
const HINT_H: f32 = 12.0;
const MIN_FADER_H: f32 = 100.0;
const MAX_FADER_H: f32 = 900.0;

/// The fader is not linear in dB: the range where a mix is set (-10 dB up
/// to the top) gets 60 % of the travel, so it can be set finely; -60 to -10
/// dB share the lower part. (level in dB, raw position 0..1) for the levels
/// below 0 dB; above 0 dB the position rises linearly to 1.0 at the top.
const TAPER: [(f32, f32); 6] = [(-60.0, 0.05), (-40.0, 0.11), (-30.0, 0.18), (-20.0, 0.27), (-10.0, 0.40), (0.0, ZERO_RAW)];
/// Raw position of 0 dB.
const ZERO_RAW: f32 = 0.80;
/// Raw positions below this are "off".
const OFF_RAW: f32 = 0.03;
/// The least distance between two labels of the scale.
const LABEL_GAP: f32 = 11.0;

/// Raw taper position of a finite level.
fn raw_pos(db: f32) -> f32 {
    let db = db.clamp(LEVEL_MIN_DB, LEVEL_MAX_DB);
    if db >= 0.0 {
        return ZERO_RAW + (1.0 - ZERO_RAW) * db / LEVEL_MAX_DB;
    }
    for w in TAPER.windows(2) {
        let ((d0, p0), (d1, p1)) = (w[0], w[1]);
        if db <= d1 {
            return p0 + (p1 - p0) * (db - d0) / (d1 - d0);
        }
    }
    ZERO_RAW
}

/// Level of a raw taper position (inverse of `raw_pos`), not rounded.
fn raw_level(pos: f32) -> f32 {
    if pos >= ZERO_RAW {
        return (pos - ZERO_RAW) / (1.0 - ZERO_RAW) * LEVEL_MAX_DB;
    }
    for w in TAPER.windows(2) {
        let ((d0, p0), (d1, p1)) = (w[0], w[1]);
        if pos <= p1 {
            return d0 + (d1 - d0) * ((pos - p0) / (p1 - p0)).max(0.0);
        }
    }
    0.0
}

/// Vertical position of a level on fader, scale and meter: 0 = bottom,
/// 1 = top (`LEVEL_MAX_DB`). "off" (and silence) is the bottom.
pub fn fader_frac(db: f32) -> f32 {
    if !db.is_finite() && db < 0.0 {
        return 0.0;
    }
    (raw_pos(db) / raw_pos(LEVEL_MAX_DB)).clamp(0.0, 1.0)
}

/// The meter shows -90 dB for silence, which is below the fader range.
fn meter_frac(db: f32) -> f32 {
    if db < LEVEL_MIN_DB {
        0.0
    } else {
        fader_frac(db)
    }
}

/// Level of a position on the fader, in steps of 0.1 dB; the bottom end
/// is "off" (`NEG_INFINITY`).
pub fn level_at_frac(frac: f32) -> f32 {
    let raw = frac.clamp(0.0, 1.0) * raw_pos(LEVEL_MAX_DB);
    if raw < OFF_RAW {
        return f32::NEG_INFINITY;
    }
    ((raw_level(raw) * 10.0).round() / 10.0).clamp(LEVEL_MIN_DB, LEVEL_MAX_DB)
}

/// One mark of the dB scale.
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    /// level; `NEG_INFINITY` for "off"
    pub db: f32,
    /// the text of a major mark, None for a minor mark
    pub label: Option<String>,
}

/// The marks of the scale: 0, -5, -10, -15, -20, -30, -40 and -inf are
/// labelled, and so are the multiples of 5 above 0 dB as far as the range
/// goes; minor marks every 1 dB down to -20 and in between further down.
pub fn scale_marks() -> Vec<Mark> {
    let mut majors: Vec<i32> = (1..=(LEVEL_MAX_DB.floor() as i32)).filter(|d| d % 5 == 0).collect();
    majors.reverse();
    majors.extend([0, -5, -10, -15, -20, -30, -40]);
    majors.retain(|d| *d as f32 <= LEVEL_MAX_DB && *d as f32 >= LEVEL_MIN_DB);
    let mut marks = Vec::new();
    let mut d = LEVEL_MAX_DB.floor() as i32;
    while d >= -20 {
        let label = majors.contains(&d).then(|| if d > 0 { format!("+{}", d) } else { d.to_string() });
        marks.push(Mark { db: d as f32, label });
        d -= 1;
    }
    for d in [-25, -30, -35, -40, -50, -60] {
        if d as f32 >= LEVEL_MIN_DB {
            let label = majors.contains(&d).then(|| d.to_string());
            marks.push(Mark { db: d as f32, label });
        }
    }
    marks.push(Mark { db: f32::NEG_INFINITY, label: Some("-\u{221e}".to_string()) });
    marks
}

/// The pointer of the pan knob: angle from straight up in radians, 135
/// degrees to each side at the ends of the range.
pub fn pan_angle(pan: i32) -> f32 {
    pan.clamp(-PAN_MAX, PAN_MAX) as f32 / PAN_MAX as f32 * 135.0_f32.to_radians()
}

/// Dragging the knob this many pixels turns it over the whole range.
const KNOB_DRAG_PX: f32 = 200.0;

/// The pan after dragging `dy` pixels downwards from `start`: dragging up
/// turns right, as the value goes up.
pub fn pan_after_drag(start: f32, dy: f32) -> i32 {
    let per_px = (2 * PAN_MAX) as f32 / KNOB_DRAG_PX;
    (start - dy * per_px).round().clamp(-PAN_MAX as f32, PAN_MAX as f32) as i32
}

/// The text under the knob: "0.0 dB", "-inf" for off. `signed` (an
/// offset of the song mix): "+2.0 dB".
pub fn fmt_db(db: f32, signed: bool) -> String {
    if db == f32::NEG_INFINITY {
        "-inf".to_string()
    } else if signed && db > 0.0 {
        format!("+{:.1} dB", db)
    } else {
        format!("{:.1} dB", db)
    }
}

/// Height of the faders for a tab with `available` height below the preset
/// bar: it uses the room, but never gets too small (then the area scrolls).
pub fn fader_height(available: f32) -> f32 {
    (available - OVERHEAD).clamp(MIN_FADER_H, MAX_FADER_H)
}

/// The sizes of the strips for an area of `width` x `height`: the 16
/// strips and the master fill the width, the faders the height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StripDims {
    pub strip_w: f32,
    pub master_w: f32,
    pub scale_w: f32,
    pub fader_w: f32,
    pub meter_w: f32,
    pub knob: f32,
    pub fader_h: f32,
}

impl StripDims {
    pub fn fit(width: f32, height: f32) -> Self {
        let master_ratio = MASTER_W / STRIP_W;
        // 16 strips with a gap after each, the extra gap, the master, and
        // a little slack so no scroll bar appears from rounding
        let fixed = STRIPS as f32 * GAP + MASTER_GAP + 2.0;
        let strip_w = ((width - fixed) / (STRIPS as f32 + master_ratio)).clamp(MIN_STRIP_W, MAX_STRIP_W);
        // the parts grow and shrink with the strip, within limits
        let k = (strip_w / STRIP_W).clamp(0.75, 1.6);
        let knob = (KNOB * k).clamp(MIN_KNOB, MAX_KNOB);
        Self {
            strip_w,
            master_w: strip_w * master_ratio,
            scale_w: (SCALE_W * k).max(MIN_SCALE_W),
            fader_w: FADER_W * k,
            meter_w: METER_W * k,
            knob,
            fader_h: fader_height(height - (knob - KNOB)),
        }
    }

    /// Height of the top part of a strip (S/M buttons and knob).
    fn top_h(&self) -> f32 {
        BUTTONS_H + self.knob
    }
}

/// `text` cut to `max` characters with "..." (counted in characters).
pub fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(3)).collect();
    format!("{}...", cut.trim_end())
}

/// Fill of the strips while they edit the song mix, so the mode is plain.
const SONG_FILL: Color32 = Color32::from_rgb(0x4a, 0x36, 0x28);

/// What the strips edit.
pub enum ConsoleMode<'a> {
    /// The rehearsal mix. `song` is the song mix of the loaded song: a
    /// strip with an offset shows it below its name. `song_name` is the
    /// loaded song (None: no song, the "Song mix" button is disabled).
    Rehearsal { song: [SongStrip; STRIPS], song_name: Option<&'a str> },
    /// The song mix of the song `name`: level and mute of the strips are
    /// the song's values (0 dB = no offset); `used` are the outputs the song
    /// has a track on (the others cannot be edited). `error`: the last save
    /// failed.
    Song { name: &'a str, strips: &'a mut [SongStrip; STRIPS], used: [bool; STRIPS], error: Option<&'a str> },
}

/// The header bar and the strips: the mixer below the player in the
/// Rehearsal tab.
#[derive(Default)]
pub struct MixerConsole {
    /// the shown meter values in dB (16 strips, master left, master right)
    shown: Option<[f32; STRIPS + 2]>,
    presets: PresetBar,
}

impl MixerConsole {
    /// Draw the header (preset bar, or the song bar in `ConsoleMode::Song`)
    /// and below it the strips, which fill the rest of `ui`. `id` is the id
    /// of the scroll area. The caller sends a changed mix to the engine and
    /// saves the config or the song.
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        id: &str,
        mixes: &mut MixLibrary,
        mut mode: ConsoleMode,
        names: &[String; STRIPS],
        levels: &Levels,
    ) -> MixerOutcome {
        let mut out = MixerOutcome::default();
        match &mut mode {
            ConsoleMode::Rehearsal { song_name, .. } => {
                let song = *song_name;
                out.changed |= self.presets.ui(ui, mixes, |ui| {
                    ui.add_space(12.0);
                    let tip = match song {
                        Some(n) => format!(
                            "Edit the mixer values of \"{}\" (the uTool2 mixer of the song, exported to the LP-16). \
                             In the rehearsal they add to the rehearsal mix, for this song only.",
                            n
                        ),
                        None => "Load a song to edit its mixer values".to_string(),
                    };
                    if ui
                        .add_enabled(song.is_some(), egui::Button::new(RichText::new("Song mix").size(13.0).color(TEXT)).fill(SONG_FILL))
                        .on_hover_text(tip.clone())
                        .on_disabled_hover_text(tip)
                        .clicked()
                    {
                        out.enter_song = true;
                    }
                });
            }
            ConsoleMode::Song { name, strips, used, error } => {
                song_bar(ui, name, *error, strips, used, &mut out);
            }
        }
        ui.add_space(6.0);
        let shown = update_shown(ui, &mut self.shown, levels);
        let dims = StripDims::fit(ui.available_width(), ui.available_height());
        egui::ScrollArea::both().id_source(id).auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                for k in 0..STRIPS {
                    let s = mixes.active().strips[k];
                    match &mut mode {
                        ConsoleMode::Rehearsal { song, .. } => {
                            let view = StripView {
                                solo: Some(s.solo),
                                mute: s.mute,
                                pan: s.pan,
                                pan_enabled: true,
                                level_db: s.level_db,
                                signed: false,
                                hint: song_hint(&song[k]),
                                fill: PANEL,
                                enabled: true,
                            };
                            let lib = &mut *mixes;
                            match strip(ui, k, &names[k], shown[k], &dims, &view) {
                                Some(Edit::Solo) => lib.active_mut().set_solo(k, !s.solo),
                                Some(Edit::Mute) => lib.active_mut().set_mute(k, !s.mute),
                                Some(Edit::Pan(p)) => lib.active_mut().set_pan(k, p),
                                Some(Edit::Level(db)) => lib.active_mut().set_level(k, db),
                                None => continue,
                            }
                            out.changed = true;
                        }
                        ConsoleMode::Song { strips, used, .. } => {
                            let own = strips[k];
                            let view = StripView {
                                solo: None,
                                mute: own.mute,
                                pan: s.pan,
                                pan_enabled: false,
                                level_db: own.level_db,
                                signed: true,
                                hint: None,
                                fill: SONG_FILL,
                                enabled: used[k],
                            };
                            match strip(ui, k, &names[k], shown[k], &dims, &view) {
                                Some(Edit::Mute) => strips[k].mute = !own.mute,
                                // off is the mute of the song, not a level
                                Some(Edit::Level(db)) => strips[k].level_db = db.max(LEVEL_MIN_DB),
                                _ => continue,
                            }
                            out.song_changed = true;
                        }
                    }
                }
                ui.add_space(MASTER_GAP);
                let (c, clip) = master(ui, [shown[STRIPS], shown[STRIPS + 1]], levels.clip, &dims, mixes);
                out.changed |= c;
                out.reset_clip |= clip;
            });
        });
        out
    }
}

/// The header in song mode: the song, Reset and the way back.
fn song_bar(
    ui: &mut Ui,
    name: &str,
    error: Option<&str>,
    strips: &mut [SongStrip; STRIPS],
    used: &[bool; STRIPS],
    out: &mut MixerOutcome,
) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Song mix").size(14.0).strong().color(ORANGE));
        ui.label(RichText::new(shorten(name, 60)).size(14.0).color(TEXT)).on_hover_text(name);
        ui.add_space(8.0);
        let neutral = strips.iter().zip(used.iter()).all(|(s, u)| !u || *s == SongStrip::default());
        if small_btn(ui, "Reset", !neutral) {
            *strips = [SongStrip::default(); STRIPS];
            out.song_changed = true;
        }
        if small_btn(ui, "Back to the rehearsal mix", true) {
            out.leave_song = true;
        }
    });
    ui.label(
        RichText::new(
            "The uTool2 mixer values of the song: saved in the song and exported to the LP-16. \
             In the rehearsal they add to the rehearsal mix (level) for this song only; pan and solo stay those of the rehearsal mix.",
        )
        .size(12.0)
        .color(DIM),
    );
    if let Some(e) = error {
        ui.label(RichText::new(format!("The song could not be saved: {}", e)).size(12.5).color(RED));
    }
}

/// The line below a strip's name in the rehearsal mix: the song's offset.
fn song_hint(s: &SongStrip) -> Option<String> {
    if s.mute {
        Some("song: mute".to_string())
    } else if s.level_db != 0.0 {
        Some(format!("song {:+.1}", s.level_db))
    } else {
        None
    }
}

fn strip_frame(ui: &mut Ui, width: f32, fill: Color32, add: impl FnOnce(&mut Ui)) {
    egui::Frame::none().fill(fill).rounding(3.0).inner_margin(Margin::same(MARGIN)).show(ui, |ui| {
        ui.set_width(width - 2.0 * MARGIN);
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            add(ui);
        });
    });
}

/// A row of fixed `width`, centred in the strip.
fn centered_row(ui: &mut Ui, width: f32, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
        add(ui);
    });
}

/// What a channel strip shows.
struct StripView {
    /// None: no solo here (the button is disabled)
    solo: Option<bool>,
    mute: bool,
    pan: i32,
    pan_enabled: bool,
    level_db: f32,
    /// the level is an offset: "+2.0 dB"
    signed: bool,
    /// a short orange line below the name
    hint: Option<String>,
    fill: Color32,
    /// false: shown, but nothing can be changed
    enabled: bool,
}

/// What the user did on a strip.
enum Edit {
    Solo,
    Mute,
    Pan(i32),
    Level(f32),
}

/// One channel strip.
fn strip(ui: &mut Ui, k: usize, track: &str, shown_db: f32, d: &StripDims, v: &StripView) -> Option<Edit> {
    let mut edit = None;
    let has_track = !track.is_empty();
    let color = if has_track { TEXT } else { DIM };
    let fader_h = d.fader_h;
    strip_frame(ui, d.strip_w, v.fill, |ui| {
        let inner = d.strip_w - 2.0 * MARGIN;
        ui.add_enabled_ui(v.enabled, |ui| {
            centered_row(ui, 2.0 * 28.0 + 3.0, |ui| {
                let solo = ui.add_enabled_ui(v.solo.is_some(), |ui| toggle(ui, "S", v.solo.unwrap_or(false), SOLO_ON)).inner;
                if solo {
                    edit = Some(Edit::Solo);
                }
                if toggle(ui, "M", v.mute, MUTE_ON) {
                    edit = Some(Edit::Mute);
                }
            });
            if let Some(pan) = pan_knob(ui, v.pan, d.knob, v.pan_enabled) {
                edit = Some(Edit::Pan(pan));
            }
            if let Some(new) = db_display(ui, &k.to_string(), inner, v.level_db, v.signed, color) {
                edit = Some(Edit::Level(new));
            }
            centered_row(ui, d.scale_w + d.fader_w + d.meter_w + 6.0, |ui| {
                scale(ui, d.scale_w, fader_h);
                if let Some(new) = fader(ui, v.level_db, d.fader_w, fader_h) {
                    edit = Some(Edit::Level(new));
                }
                meter_sized(ui, shown_db, d.meter_w, fader_h, HANDLE_H / 2.0, meter_frac);
            });
        });
        ui.label(RichText::new(format!("{}", k + 1)).size(15.0).strong().color(color));
        let name = if has_track { track } else { "\u{2014}" };
        ui.add_sized([inner, 14.0], egui::Label::new(RichText::new(name).size(11.0).color(color)).truncate(true))
            .on_hover_text(if has_track { track.to_string() } else { "No track on this output in the song".to_string() });
        let hint = v.hint.as_deref().unwrap_or("");
        ui.add_sized([inner, HINT_H], egui::Label::new(RichText::new(hint).size(10.5).color(ORANGE)).truncate(true))
            .on_hover_text("The song has its own mixer value on this output (\"Song mix\"); it adds to this strip.");
    });
    edit
}

/// The master strip: clip LED, level display, fader, scale and the L/R
/// meters. Returns (changed, clip LED clicked).
fn master(ui: &mut Ui, shown: [f32; 2], clip: bool, d: &StripDims, lib: &mut MixLibrary) -> (bool, bool) {
    let (mut changed, mut reset) = (false, false);
    let master_db = lib.active().master_db;
    let fader_h = d.fader_h;
    strip_frame(ui, d.master_w, PANEL, |ui| {
        let inner = d.master_w - 2.0 * MARGIN;
        let top = ui.cursor().top();
        ui.label(RichText::new("Master").size(14.0).strong().color(ORANGE));
        let (rect, resp) = ui.allocate_exact_size(vec2(60.0, 18.0), Sense::click());
        let p = ui.painter();
        p.circle_filled(pos2(rect.left() + 9.0, rect.center().y), 5.0, if clip { RED } else { Color32::from_rgb(0x50, 0x28, 0x28) });
        p.text(
            pos2(rect.left() + 20.0, rect.center().y),
            Align2::LEFT_CENTER,
            "CLIP",
            FontId::proportional(11.0),
            if clip { RED } else { DIM },
        );
        if resp.on_hover_text("The master was limited. Click to reset.").clicked() {
            reset = true;
        }
        // as tall as the S/M buttons and the knob of the other strips
        let used = ui.cursor().top() - top;
        ui.add_space((d.top_h() - used).max(0.0));
        if let Some(new) = db_display(ui, "master", inner, master_db, false, TEXT) {
            lib.active_mut().set_master(new);
            changed = true;
        }
        centered_row(ui, d.scale_w + d.fader_w + 2.0 * d.meter_w + 9.0, |ui| {
            scale(ui, d.scale_w, fader_h);
            if let Some(new) = fader(ui, master_db, d.fader_w, fader_h) {
                lib.active_mut().set_master(new);
                changed = true;
            }
            meter_sized(ui, shown[0], d.meter_w, fader_h, HANDLE_H / 2.0, meter_frac);
            meter_sized(ui, shown[1], d.meter_w, fader_h, HANDLE_H / 2.0, meter_frac);
        });
        ui.label(RichText::new("L    R").size(11.0).color(DIM));
        ui.add_sized([inner, 14.0], egui::Label::new(RichText::new("Master").size(11.0).color(TEXT)));
    });
    (changed, reset)
}

/// The level text in a dark field. A double-click turns it into a text
/// field to type the level ("-0.5", "-inf"); Enter or a click elsewhere
/// takes it, Esc cancels. Returns the typed level if it differs. `key`
/// tells the strips apart.
fn db_display(ui: &mut Ui, key: &str, width: f32, db: f32, signed: bool, color: Color32) -> Option<f32> {
    let id = ui.id().with(("level_edit", key));
    let editing = ui.data(|d| d.get_temp::<String>(id));
    let Some(mut text) = editing else {
        let (rect, resp) = ui.allocate_exact_size(vec2(width, 18.0), Sense::click());
        let resp = resp.on_hover_text("Double-click: type the level (e.g. -0.5, -inf)");
        let p = ui.painter();
        p.rect_filled(rect, 2.0, Color32::from_rgb(0x4a, 0x4a, 0x4a));
        if resp.hovered() {
            p.rect_stroke(rect, 2.0, Stroke::new(1.0_f32, LINE));
        }
        p.text(rect.center(), Align2::CENTER_CENTER, fmt_db(db, signed), FontId::monospace(12.0), color);
        if resp.double_clicked() && resp.enabled() {
            let start = if db == f32::NEG_INFINITY { "-inf".to_string() } else { format!("{:.1}", db) };
            ui.data_mut(|d| {
                d.insert_temp(id, start);
                d.insert_temp(id.with("new"), true);
            });
            ui.ctx().request_repaint();
        }
        return None;
    };
    let edit_id = id.with("field");
    let first = ui.data_mut(|d| d.remove_temp::<bool>(id.with("new"))).unwrap_or(false);
    let resp = ui.add_sized(
        [width, 18.0],
        egui::TextEdit::singleline(&mut text)
            .id(edit_id)
            .font(FontId::monospace(12.0))
            .horizontal_align(egui::Align::Center)
            .margin(vec2(2.0, 1.0)),
    );
    if first {
        // focus and select everything, so typing replaces the value
        resp.request_focus();
        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), edit_id) {
            let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(text.chars().count()));
            state.cursor.set_char_range(Some(all));
            state.store(ui.ctx(), edit_id);
        }
        ui.data_mut(|d| d.insert_temp(id, text));
        return None;
    }
    if !resp.lost_focus() {
        ui.data_mut(|d| d.insert_temp(id, text));
        return None;
    }
    // done: Esc cancels (and is taken, so it does not stop the player)
    ui.data_mut(|d| d.remove::<String>(id));
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        return None;
    }
    parse_level(&text).filter(|v| *v != db)
}

/// The y of position `frac` on a track of a fader in `rect`.
fn track_y(rect: Rect, frac: f32) -> f32 {
    let track = rect.shrink2(vec2(0.0, HANDLE_H / 2.0));
    track.bottom() - track.height() * frac
}

/// The dB scale: ticks and labels at the same heights as the fader.
fn scale(ui: &mut Ui, width: f32, height: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let p = ui.painter();
    // a label too close to the one above is left out (short faders)
    let mut last_label_y = f32::NEG_INFINITY;
    for m in scale_marks() {
        let y = track_y(rect, fader_frac(m.db));
        let len = if m.label.is_some() { 7.0 } else { 3.5 };
        let x = rect.right();
        p.line_segment([pos2(x - len, y), pos2(x, y)], Stroke::new(1.0_f32, if m.label.is_some() { TEXT } else { LINE }));
        if let Some(text) = &m.label {
            if y - last_label_y >= LABEL_GAP {
                p.text(pos2(x - 9.0, y), Align2::RIGHT_CENTER, text, FontId::proportional(10.0), DIM);
                last_label_y = y;
            }
        }
    }
}

/// The fader: drag or click to set the level, double-click for 0 dB.
/// Returns the new level if it changed.
fn fader(ui: &mut Ui, level_db: f32, width: f32, height: f32) -> Option<f32> {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click_and_drag());
    let resp = resp.on_hover_text("Level. Double-click: 0 dB");
    let track = rect.shrink2(vec2(0.0, HANDLE_H / 2.0));
    let p = ui.painter();
    // the groove
    let groove = Rect::from_center_size(pos2(rect.center().x, track.center().y), vec2(6.0, track.height() + 4.0));
    p.rect_filled(groove, 2.0, Color32::BLACK);
    // the handle with grip lines
    let y = track_y(rect, fader_frac(level_db));
    let h = Rect::from_center_size(pos2(rect.center().x, y), vec2(width, HANDLE_H));
    p.rect_filled(h, 2.0, Color32::from_rgb(0x58, 0x58, 0x58));
    p.rect_stroke(h, 2.0, Stroke::new(1.0_f32, if resp.hovered() || resp.dragged() { ORANGE } else { LINE }));
    for dy in [-4.0_f32, -1.0, 2.0] {
        p.line_segment([pos2(h.left() + 3.0, y + dy), pos2(h.right() - 3.0, y + dy)], Stroke::new(1.0_f32, Color32::from_rgb(0x20, 0x20, 0x20)));
    }
    p.line_segment([pos2(h.left() + 3.0, y + 4.0), pos2(h.right() - 3.0, y + 4.0)], Stroke::new(1.0_f32, TEXT));
    if double_clicked(ui, &resp) {
        return Some(0.0).filter(|_| level_db != 0.0);
    }
    if resp.dragged() || resp.clicked() {
        let pos = resp.interact_pointer_pos()?;
        let frac = (track.bottom() - pos.y) / track.height().max(1.0);
        return Some(level_at_frac(frac)).filter(|n| *n != level_db);
    }
    None
}

/// The pan knob: drag up or down to turn, double-click for the center.
/// `enabled` false: it only shows the pan (song mode: the pan is that of
/// the rehearsal mix).
fn pan_knob(ui: &mut Ui, pan: i32, size: f32, enabled: bool) -> Option<i32> {
    let sense = if enabled { Sense::click_and_drag() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), sense);
    let resp = if enabled {
        resp.on_hover_text(format!("Pan: {}. Drag up or down, double-click: center", fmt_pan(pan)))
    } else {
        resp.on_hover_text(format!("Pan: {} (of the rehearsal mix; the song's pan is not used)", fmt_pan(pan)))
    };
    let c = rect.center();
    let r = size / 2.0 - 3.0;
    let p = ui.painter();
    p.circle_filled(c, r, Color32::from_rgb(0x1c, 0x1c, 0x1c));
    let active = enabled && (resp.hovered() || resp.dragged());
    p.circle_stroke(c, r, Stroke::new(1.5_f32, if active { ORANGE } else { LINE }));
    // the top mark (center)
    p.line_segment([pos2(c.x, c.y - r - 2.0), pos2(c.x, c.y - r + 2.0)], Stroke::new(1.0_f32, DIM));
    let a = pan_angle(pan);
    let dir = vec2(a.sin(), -a.cos());
    let pointer = if !enabled { DIM } else if pan == 0 { TEXT } else { ORANGE };
    p.line_segment([c + dir * (r * 0.25), c + dir * (r - 3.0)], Stroke::new(2.5_f32, pointer));
    if !enabled {
        return None;
    }
    if double_clicked(ui, &resp) {
        return Some(0).filter(|_| pan != 0);
    }
    if resp.drag_started() {
        ui.data_mut(|d| d.insert_temp(resp.id, pan as f32));
    }
    if resp.dragged() {
        let start = ui.data(|d| d.get_temp::<f32>(resp.id)).unwrap_or(pan as f32);
        let origin = ui.input(|i| i.pointer.press_origin())?;
        let now = resp.interact_pointer_pos()?;
        return Some(pan_after_drag(start, now.y - origin.y)).filter(|n| *n != pan);
    }
    None
}

// ---------------------------------------------------------------------------
// The backup mixer of the "Export Stereo-MP3" tab: one side (left or right)
// of the backup mix, a row of compact strips (mute, level, fader and meter,
// no pan) and a master. Two of them are stacked, so they are lower than the
// rehearsal mixer.
// ---------------------------------------------------------------------------

/// What a backup strip needs besides its fader and its name.
const BACKUP_OVERHEAD: f32 = 106.0;
const BACKUP_MIN_FADER_H: f32 = 70.0;
/// Font of the strip names; they wrap, so the whole name is shown.
const NAME_FONT: f32 = 12.0;
const NAME_ROW_H: f32 = 15.0;

/// The name centred and wrapped to `width`.
fn name_galley(ui: &Ui, text: &str, width: f32, color: Color32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(text.to_string(), FontId::proportional(NAME_FONT), color, width);
    job.halign = egui::Align::Center;
    ui.fonts(|f| f.layout_job(job))
}

/// A strip name of `width` x `height`, wrapped; `edit` is the text the
/// field starts with. A double-click turns it into a text field (Enter or
/// a click elsewhere takes it, Esc cancels); returns the typed text. `key`
/// tells the fields apart.
#[allow(clippy::too_many_arguments)]
fn name_field(ui: &mut Ui, key: &str, width: f32, height: f32, name: &str, edit: &str, color: Color32, tip: &str) -> Option<String> {
    let id = ui.id().with(("name_edit", key));
    if let Some(mut text) = ui.data(|d| d.get_temp::<String>(id)) {
        let edit_id = id.with("field");
        let first = ui.data_mut(|d| d.remove_temp::<bool>(id.with("new"))).unwrap_or(false);
        let resp = ui.add_sized(
            [width, height.max(20.0)],
            egui::TextEdit::multiline(&mut text)
                .id(edit_id)
                .font(FontId::proportional(NAME_FONT))
                .horizontal_align(egui::Align::Center)
                .margin(vec2(2.0, 1.0)),
        );
        if first {
            resp.request_focus();
            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), edit_id) {
                let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(text.chars().count()));
                state.cursor.set_char_range(Some(all));
                state.store(ui.ctx(), edit_id);
            }
            ui.data_mut(|d| d.insert_temp(id, text));
            return None;
        }
        // Enter takes the name (the field is multi-line only to wrap)
        let enter = text.contains('\n');
        if !resp.lost_focus() && !enter {
            ui.data_mut(|d| d.insert_temp(id, text));
            return None;
        }
        ui.data_mut(|d| d.remove::<String>(id));
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            return None;
        }
        return Some(text.replace('\n', ""));
    }
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    let resp = resp.on_hover_text(tip);
    if resp.hovered() {
        ui.painter().rect_stroke(rect, 2.0, Stroke::new(1.0_f32, LINE));
    }
    let galley = name_galley(ui, name, width, color);
    ui.painter().galley(pos2(rect.center().x, rect.top()), galley, color);
    if resp.double_clicked() && resp.enabled() {
        ui.data_mut(|d| {
            d.insert_temp(id, edit.to_string());
            d.insert_temp(id.with("new"), true);
        });
        ui.ctx().request_repaint();
    }
    None
}

/// Draw one side of the backup mix into an area of `height`; `id` tells the
/// two sides apart, `title` is shown in the master strip. True if a value
/// changed. `fill` colours the strips that reach this side. `meters`: the
/// shown level per strip and of this side's master (dB, from the preview
/// player). `names` are the own names of the strips (edited with a
/// double-click, shared by both sides), `auto` the track names of the songs
/// (shown while there is no own name).
pub fn backup_side(
    ui: &mut Ui,
    id: &str,
    title: &str,
    height: f32,
    side: &mut BackupSide,
    names: &mut [String; STRIPS],
    auto: &[String; STRIPS],
    fill: Color32,
    enabled: bool,
    meters: ([f32; STRIPS], f32),
) -> bool {
    let mut changed = false;
    let mut d = StripDims::fit(ui.available_width(), height);
    let inner = d.strip_w - 2.0 * MARGIN;
    let shown: [String; STRIPS] = std::array::from_fn(|k| {
        let n = if names[k].is_empty() { &auto[k] } else { &names[k] };
        if n.is_empty() { "\u{2014}".to_string() } else { n.clone() }
    });
    // every strip gets the lines of the longest name
    let rows = shown.iter().map(|n| name_galley(ui, n, inner, TEXT).rows.len()).max().unwrap_or(1).max(1);
    let name_h = rows as f32 * NAME_ROW_H + 2.0;
    d.fader_h = (height - BACKUP_OVERHEAD - name_h).clamp(BACKUP_MIN_FADER_H, MAX_FADER_H);
    egui::ScrollArea::horizontal().id_source(id).auto_shrink([false, true]).show(ui, |ui| {
        ui.add_enabled_ui(enabled, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                for k in 0..STRIPS {
                    let used = side.uses(k);
                    let s = side.strips[k];
                    let has_track = !names[k].is_empty() || !auto[k].is_empty();
                    let color = if has_track { TEXT } else { DIM };
                    strip_frame(ui, d.strip_w, if used { fill } else { PANEL }, |ui| {
                        centered_row(ui, 28.0, |ui| {
                            if toggle(ui, "M", s.mute, MUTE_ON) {
                                side.set_mute(k, !s.mute);
                                changed = true;
                            }
                        });
                        if let Some(new) = db_display(ui, &format!("{}{}", id, k), inner, s.level_db, false, color) {
                            side.set_level(k, new);
                            changed = true;
                        }
                        centered_row(ui, d.scale_w + d.fader_w + d.meter_w + 6.0, |ui| {
                            scale(ui, d.scale_w, d.fader_h);
                            if let Some(new) = fader(ui, s.level_db, d.fader_w, d.fader_h) {
                                side.set_level(k, new);
                                changed = true;
                            }
                            meter_sized(ui, meters.0[k], d.meter_w, d.fader_h, HANDLE_H / 2.0, meter_frac);
                        });
                        ui.label(RichText::new(format!("{}", k + 1)).size(15.0).strong().color(color));
                        let tip = match (names[k].is_empty(), auto[k].is_empty()) {
                            (true, true) => "No track on this output in the songs. Double-click: name it".to_string(),
                            (true, false) => format!("{}\nDouble-click: rename (for all songs)", auto[k]),
                            (false, true) => format!("{}\nDouble-click: rename; empty = no name", names[k]),
                            (false, false) => format!("{}\nIn the songs: {}\nDouble-click: rename; empty = the song name", names[k], auto[k]),
                        };
                        let edit_text = if names[k].is_empty() { auto[k].as_str() } else { names[k].as_str() };
                        if let Some(text) = name_field(ui, &format!("{}{}", id, k), inner, name_h, &shown[k], edit_text, color, &tip) {
                            let t = lp16_tool::backup_mix::clean_label(&text);
                            let new = if t == auto[k].trim() { String::new() } else { t };
                            if new != names[k] {
                                names[k] = new;
                                changed = true;
                            }
                        }
                    });
                }
                ui.add_space(MASTER_GAP - GAP);
                strip_frame(ui, d.master_w, PANEL, |ui| {
                    let inner = d.master_w - 2.0 * MARGIN;
                    ui.add_space(23.0);
                    if let Some(new) = db_display(ui, &format!("{}master", id), inner, side.master_db, false, TEXT) {
                        side.set_master(new);
                        changed = true;
                    }
                    centered_row(ui, d.scale_w + d.fader_w + d.meter_w + 6.0, |ui| {
                        scale(ui, d.scale_w, d.fader_h);
                        if let Some(new) = fader(ui, side.master_db, d.fader_w, d.fader_h) {
                            side.set_master(new);
                            changed = true;
                        }
                        meter_sized(ui, meters.1, d.meter_w, d.fader_h, HANDLE_H / 2.0, meter_frac);
                    });
                    ui.label(RichText::new("Master").size(14.0).strong().color(ORANGE));
                    ui.add_sized([inner, name_h], egui::Label::new(RichText::new(title).size(NAME_FONT).color(TEXT)));
                });
            });
        });
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui_mixer::FLOOR_DB;

    #[test]
    fn fader_frac_runs_from_off_to_the_top() {
        assert_eq!(fader_frac(f32::NEG_INFINITY), 0.0);
        assert!((fader_frac(LEVEL_MAX_DB) - 1.0).abs() < 1e-6);
        assert!(fader_frac(LEVEL_MIN_DB) > 0.0, "the lowest level is above off");
        // 0 dB is in the upper part, below the top
        let z = fader_frac(0.0);
        assert!(z > 0.7 && z < 1.0, "{}", z);
    }

    #[test]
    fn the_range_from_minus_10_db_up_gets_most_of_the_travel() {
        let upper = 1.0 - fader_frac(-10.0);
        assert!(upper >= 0.55, "-10 dB .. top: {}", upper);
        // finer there than below: more travel per dB
        let per_db_upper = upper / (LEVEL_MAX_DB + 10.0);
        let per_db_lower = (fader_frac(-10.0) - fader_frac(LEVEL_MIN_DB)) / (-10.0 - LEVEL_MIN_DB);
        assert!(per_db_upper > 3.0 * per_db_lower, "{} vs {}", per_db_upper, per_db_lower);
        // a 400 px fader moves at least 10 px per dB around 0 dB
        assert!((fader_frac(0.0) - fader_frac(-1.0)) * 400.0 >= 10.0);
    }

    #[test]
    fn fader_frac_rises_with_the_level() {
        let mut last = -1.0;
        let mut db = LEVEL_MIN_DB;
        while db <= LEVEL_MAX_DB {
            let f = fader_frac(db);
            assert!(f > last, "{} dB", db);
            last = f;
            db += 0.1;
        }
    }

    #[test]
    fn fader_position_roundtrips_to_the_level() {
        let mut db = LEVEL_MIN_DB;
        while db <= LEVEL_MAX_DB {
            let back = level_at_frac(fader_frac(db));
            assert!((back - db).abs() < 0.051, "{} -> {}", db, back);
            db += 0.1;
        }
        assert_eq!(level_at_frac(fader_frac(0.0)), 0.0);
        assert_eq!(level_at_frac(fader_frac(-10.0)), -10.0);
    }

    #[test]
    fn the_bottom_of_the_fader_is_off_and_the_ends_are_clamped() {
        assert_eq!(level_at_frac(0.0), f32::NEG_INFINITY);
        assert_eq!(level_at_frac(-0.5), f32::NEG_INFINITY);
        assert_eq!(level_at_frac(1.0), LEVEL_MAX_DB);
        assert_eq!(level_at_frac(7.0), LEVEL_MAX_DB);
        // the lowest level is reachable just above off
        assert_eq!(level_at_frac(fader_frac(LEVEL_MIN_DB)), LEVEL_MIN_DB);
        assert_eq!(level_at_frac(fader_frac(LEVEL_MIN_DB) - 0.01), LEVEL_MIN_DB);
    }

    #[test]
    fn meter_shows_silence_at_the_bottom() {
        assert_eq!(meter_frac(FLOOR_DB), 0.0);
        assert_eq!(meter_frac(f32::NEG_INFINITY), 0.0);
        assert!((meter_frac(0.0) - fader_frac(0.0)).abs() < 1e-6);
    }

    #[test]
    fn scale_has_the_labels_of_the_original() {
        let marks = scale_marks();
        let labels: Vec<String> = marks.iter().filter_map(|m| m.label.clone()).collect();
        // positive values come first, because the range goes beyond 0 dB
        assert_eq!(labels, ["+5", "0", "-5", "-10", "-15", "-20", "-30", "-40", "-\u{221e}"]);
        // top to bottom, every mark inside the range, "off" last
        let finite: Vec<f32> = marks.iter().map(|m| m.db).filter(|d| d.is_finite()).collect();
        assert!(finite.windows(2).all(|w| w[0] > w[1]));
        assert!(finite.iter().all(|d| *d >= LEVEL_MIN_DB && *d <= LEVEL_MAX_DB));
        assert_eq!(marks.last().unwrap().db, f32::NEG_INFINITY);
        // and from the bottom of the fader to the top
        let ys: Vec<f32> = marks.iter().map(|m| fader_frac(m.db)).collect();
        assert!(ys.windows(2).all(|w| w[0] > w[1]));
    }

    #[test]
    fn pan_pointer_angle() {
        assert_eq!(pan_angle(0), 0.0);
        assert!((pan_angle(PAN_MAX) - 135.0_f32.to_radians()).abs() < 1e-6);
        assert!((pan_angle(-PAN_MAX) + 135.0_f32.to_radians()).abs() < 1e-6);
        assert_eq!(pan_angle(500), pan_angle(PAN_MAX));
    }

    #[test]
    fn dragging_the_knob() {
        // up (negative dy) turns right, down turns left
        assert!(pan_after_drag(0.0, -20.0) > 0);
        assert!(pan_after_drag(0.0, 20.0) < 0);
        assert_eq!(pan_after_drag(0.0, 0.0), 0);
        // the whole range in 200 px, and no further
        assert_eq!(pan_after_drag(-127.0, -200.0), 127);
        assert_eq!(pan_after_drag(0.0, -1000.0), PAN_MAX);
        assert_eq!(pan_after_drag(0.0, 1000.0), -PAN_MAX);
        // small movements add up from the start value, not from a rounded one
        assert_eq!(pan_after_drag(10.0, -10.0), 23);
    }

    #[test]
    fn level_text() {
        assert_eq!(fmt_db(0.0, false), "0.0 dB");
        assert_eq!(fmt_db(-12.34, false), "-12.3 dB");
        assert_eq!(fmt_db(f32::NEG_INFINITY, false), "-inf");
        assert_eq!(fmt_db(2.0, true), "+2.0 dB");
        assert_eq!(fmt_db(0.0, true), "0.0 dB");
        assert_eq!(fmt_db(-1.5, true), "-1.5 dB");
    }

    #[test]
    fn fader_height_uses_the_room_with_a_minimum() {
        assert_eq!(fader_height(10.0), MIN_FADER_H);
        assert_eq!(fader_height(OVERHEAD + 400.0), 400.0);
        assert_eq!(fader_height(100000.0), MAX_FADER_H);
    }

    #[test]
    fn the_strips_fill_the_width_between_the_limits() {
        // 16 strips, the gaps and the master use the whole width
        let d = StripDims::fit(1800.0, 600.0);
        let used = STRIPS as f32 * (d.strip_w + GAP) + MASTER_GAP + d.master_w;
        assert!(used <= 1800.0 && used > 1790.0, "{}", used);
        assert!(d.strip_w > STRIP_W);
        // a narrower frame gives narrower strips, within the limits
        assert!(StripDims::fit(1400.0, 600.0).strip_w < d.strip_w);
        assert_eq!(StripDims::fit(300.0, 600.0).strip_w, MIN_STRIP_W);
        assert_eq!(StripDims::fit(10000.0, 600.0).strip_w, MAX_STRIP_W);
    }

    #[test]
    fn the_parts_of_the_narrowest_strip_fit_into_it() {
        let d = StripDims::fit(0.0, 600.0);
        let inner = d.strip_w - 2.0 * MARGIN;
        assert!(d.scale_w + d.fader_w + d.meter_w + 6.0 <= inner);
        assert!(2.0 * 28.0 + 3.0 <= inner, "S and M buttons");
        assert!(d.knob <= inner);
        let master_inner = d.master_w - 2.0 * MARGIN;
        assert!(d.scale_w + d.fader_w + 2.0 * d.meter_w + 9.0 <= master_inner);
    }

    #[test]
    fn the_faders_follow_the_height() {
        let low = StripDims::fit(1600.0, 400.0);
        let high = StripDims::fit(1600.0, 800.0);
        assert!((high.fader_h - low.fader_h - 400.0).abs() < 1e-3);
        assert_eq!(StripDims::fit(1600.0, 0.0).fader_h, MIN_FADER_H);
    }

    #[test]
    fn shorten_counts_characters() {
        assert_eq!(shorten("abc", 5), "abc");
        assert_eq!(shorten("abcdefghij", 8), "abcde...");
        assert_eq!(shorten("\u{e4}\u{f6}\u{fc}\u{e4}\u{f6}\u{fc}", 5), "\u{e4}\u{f6}...");
    }
}

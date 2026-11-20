//! The "Mixer" tab: the rehearsal mix in the style of the uTool2 mixer, with
//! narrow, tall channel strips (16 strips and the master, side by side in a
//! horizontal scroll area), the preset bar on top and the player transport
//! at the bottom.
//!
//! It edits the same global rehearsal mix as the small mixer in the
//! Rehearsal tab (same `MixLibrary`, same commands to the engine, same
//! saving) and drives the same `RehearsalPanel`, so a song chosen or played
//! in one tab is the same state in the other. It never writes into songs.
//! All the logic is in the library; the helpers here are only about
//! drawing (fader taper, scale marks, knob angle).

use crate::gui_app::{DIM, LINE, ORANGE, PANEL, RED, TEXT};
use crate::gui_mixer::{double_clicked, fmt_pan, meter_sized, toggle, update_shown, PresetBar, MUTE_ON, SOLO_ON};
use crate::gui_rehearsal::{RehearsalPanel, TRANSPORT_COMPACT};
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Margin, Rect, RichText, Sense, Stroke, Ui};
use lp16_tool::mix::{MixLibrary, LEVEL_MAX_DB, LEVEL_MIN_DB, PAN_MAX, STRIPS};
use lp16_tool::rehearsal::{PlayState, Status};

/// Outer width of a channel strip (frame margins included) and of the master.
const STRIP_W: f32 = 92.0;
const MASTER_W: f32 = 116.0;
const MARGIN: f32 = 4.0;
/// Width of the dB scale, the fader and one meter.
const SCALE_W: f32 = 30.0;
const FADER_W: f32 = 22.0;
const METER_W: f32 = 10.0;
/// Height of the fader handle; the track is inset by half of it.
const HANDLE_H: f32 = 14.0;
/// Size of the pan knob.
const KNOB: f32 = 40.0;
/// Height of the top part of a strip (S/M buttons and knob).
const TOP_H: f32 = 63.0;
/// What the rest of a strip needs besides the fader (texts, spacing,
/// frame, the scroll bar), and the lowest fader the tab draws.
const OVERHEAD: f32 = 170.0;
const MIN_FADER_H: f32 = 140.0;
const MAX_FADER_H: f32 = 900.0;

/// The fader is not linear in dB: fine steps near 0 dB, like a real fader.
/// (level in dB, raw position 0..1) for the levels below 0 dB; above 0 dB
/// the position rises linearly to 1.0 at +6 dB.
const TAPER: [(f32, f32); 6] = [(-60.0, 0.08), (-40.0, 0.22), (-30.0, 0.34), (-20.0, 0.50), (-10.0, 0.72), (0.0, 0.90)];
/// Raw positions below this are "off".
const OFF_RAW: f32 = 0.04;

/// Raw taper position of a finite level.
fn raw_pos(db: f32) -> f32 {
    let db = db.clamp(LEVEL_MIN_DB, LEVEL_MAX_DB);
    if db >= 0.0 {
        return 0.90 + 0.10 * db / 6.0;
    }
    for w in TAPER.windows(2) {
        let ((d0, p0), (d1, p1)) = (w[0], w[1]);
        if db <= d1 {
            return p0 + (p1 - p0) * (db - d0) / (d1 - d0);
        }
    }
    0.90
}

/// Level of a raw taper position (inverse of `raw_pos`), not rounded.
fn raw_level(pos: f32) -> f32 {
    if pos >= 0.90 {
        return (pos - 0.90) / 0.10 * 6.0;
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

/// The text under the knob: "0.0 dB", "-inf" for off.
pub fn fmt_db(db: f32) -> String {
    if db == f32::NEG_INFINITY {
        "-inf".to_string()
    } else {
        format!("{:.1} dB", db)
    }
}

/// Height of the faders for a tab with `available` height below the preset
/// bar: it uses the room, but never gets too small (then the area scrolls).
pub fn fader_height(available: f32) -> f32 {
    (available - OVERHEAD).clamp(MIN_FADER_H, MAX_FADER_H)
}

/// `text` cut to `max` characters with "..." (counted in characters).
pub fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(3)).collect();
    format!("{}...", cut.trim_end())
}

#[derive(Default)]
pub struct MixerTab {
    /// the shown meter values in dB (16 strips, master left, master right)
    shown: Option<[f32; STRIPS + 2]>,
    presets: PresetBar,
}

impl MixerTab {
    /// Draw the tab. Returns true if the mix (values or presets) changed, so
    /// the caller saves the config; the new mix has been sent to the engine.
    pub fn ui(&mut self, ui: &mut Ui, player: &mut RehearsalPanel, mixes: &mut MixLibrary) -> bool {
        let status = player.begin_frame(ui.ctx());
        let levels = player.take_levels();
        // the same shortcuts as in the Rehearsal tab
        if let Some(st) = &status {
            player.keys(ui, st);
        }
        egui::TopBottomPanel::bottom("mixer_tab_transport")
            .frame(egui::Frame::none().inner_margin(Margin { left: 0.0, right: 0.0, top: 6.0, bottom: 0.0 }))
            .show_separator_line(true)
            .show_inside(ui, |ui| transport_bar(ui, player, status.as_ref()));
        let mut changed = self.presets.ui(ui, mixes);
        ui.add_space(6.0);
        let shown = update_shown(ui, &mut self.shown, &levels);
        let fader_h = fader_height(ui.available_height());
        let names = player.strip_names().clone();
        let mut reset_clip = false;
        egui::ScrollArea::both().id_source("mixer_tab_scroll").auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for k in 0..STRIPS {
                    changed |= strip(ui, k, &names[k], shown[k], fader_h, mixes);
                }
                ui.add_space(8.0);
                let (c, clip) = master(ui, [shown[STRIPS], shown[STRIPS + 1]], levels.clip, fader_h, mixes);
                changed |= c;
                reset_clip |= clip;
            });
        });
        if reset_clip {
            player.reset_clip();
        }
        if changed {
            player.set_mix(mixes.active().params());
        }
        changed
    }
}

fn strip_frame(ui: &mut Ui, width: f32, add: impl FnOnce(&mut Ui)) {
    egui::Frame::none().fill(PANEL).rounding(3.0).inner_margin(Margin::same(MARGIN)).show(ui, |ui| {
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

/// One channel strip; true if something changed.
fn strip(ui: &mut Ui, k: usize, track: &str, shown_db: f32, fader_h: f32, lib: &mut MixLibrary) -> bool {
    let mut changed = false;
    let s = lib.active().strips[k];
    let has_track = !track.is_empty();
    let color = if has_track { TEXT } else { DIM };
    strip_frame(ui, STRIP_W, |ui| {
        let inner = STRIP_W - 2.0 * MARGIN;
        centered_row(ui, 2.0 * 28.0 + 3.0, |ui| {
            if toggle(ui, "S", s.solo, SOLO_ON) {
                lib.active_mut().set_solo(k, !s.solo);
                changed = true;
            }
            if toggle(ui, "M", s.mute, MUTE_ON) {
                lib.active_mut().set_mute(k, !s.mute);
                changed = true;
            }
        });
        if let Some(pan) = pan_knob(ui, s.pan) {
            lib.active_mut().set_pan(k, pan);
            changed = true;
        }
        db_display(ui, inner, s.level_db, color);
        centered_row(ui, SCALE_W + FADER_W + METER_W + 6.0, |ui| {
            scale(ui, fader_h);
            if let Some(new) = fader(ui, s.level_db, fader_h) {
                lib.active_mut().set_level(k, new);
                changed = true;
            }
            meter_sized(ui, shown_db, METER_W, fader_h, HANDLE_H / 2.0, meter_frac);
        });
        ui.label(RichText::new(format!("{}", k + 1)).size(15.0).strong().color(color));
        let name = if has_track { track } else { "\u{2014}" };
        ui.add_sized([inner, 14.0], egui::Label::new(RichText::new(name).size(11.0).color(color)).truncate(true))
            .on_hover_text(if has_track { track.to_string() } else { "No track on this output in the song".to_string() });
    });
    changed
}

/// The master strip: clip LED, level display, fader, scale and the L/R
/// meters. Returns (changed, clip LED clicked).
fn master(ui: &mut Ui, shown: [f32; 2], clip: bool, fader_h: f32, lib: &mut MixLibrary) -> (bool, bool) {
    let (mut changed, mut reset) = (false, false);
    let master_db = lib.active().master_db;
    strip_frame(ui, MASTER_W, |ui| {
        let inner = MASTER_W - 2.0 * MARGIN;
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
        ui.add_space((TOP_H - used).max(0.0));
        db_display(ui, inner, master_db, TEXT);
        centered_row(ui, SCALE_W + FADER_W + 2.0 * METER_W + 9.0, |ui| {
            scale(ui, fader_h);
            if let Some(new) = fader(ui, master_db, fader_h) {
                lib.active_mut().set_master(new);
                changed = true;
            }
            meter_sized(ui, shown[0], METER_W, fader_h, HANDLE_H / 2.0, meter_frac);
            meter_sized(ui, shown[1], METER_W, fader_h, HANDLE_H / 2.0, meter_frac);
        });
        ui.label(RichText::new("L    R").size(11.0).color(DIM));
        ui.add_sized([inner, 14.0], egui::Label::new(RichText::new("Master").size(11.0).color(TEXT)));
    });
    (changed, reset)
}

/// The level text in a dark field.
fn db_display(ui: &mut Ui, width: f32, db: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 18.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 2.0, Color32::from_rgb(0x4a, 0x4a, 0x4a));
    p.text(rect.center(), Align2::CENTER_CENTER, fmt_db(db), FontId::monospace(12.0), color);
}

/// The y of position `frac` on a track of a fader in `rect`.
fn track_y(rect: Rect, frac: f32) -> f32 {
    let track = rect.shrink2(vec2(0.0, HANDLE_H / 2.0));
    track.bottom() - track.height() * frac
}

/// The dB scale: ticks and labels at the same heights as the fader.
fn scale(ui: &mut Ui, height: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(SCALE_W, height), Sense::hover());
    let p = ui.painter();
    for m in scale_marks() {
        let y = track_y(rect, fader_frac(m.db));
        let len = if m.label.is_some() { 7.0 } else { 3.5 };
        let x = rect.right();
        p.line_segment([pos2(x - len, y), pos2(x, y)], Stroke::new(1.0_f32, if m.label.is_some() { TEXT } else { LINE }));
        if let Some(text) = &m.label {
            p.text(pos2(x - 9.0, y), Align2::RIGHT_CENTER, text, FontId::proportional(10.0), DIM);
        }
    }
}

/// The fader: drag or click to set the level, double-click for 0 dB.
/// Returns the new level if it changed.
fn fader(ui: &mut Ui, level_db: f32, height: f32) -> Option<f32> {
    let (rect, resp) = ui.allocate_exact_size(vec2(FADER_W, height), Sense::click_and_drag());
    let resp = resp.on_hover_text("Level. Double-click: 0 dB");
    let track = rect.shrink2(vec2(0.0, HANDLE_H / 2.0));
    let p = ui.painter();
    // the groove
    let groove = Rect::from_center_size(pos2(rect.center().x, track.center().y), vec2(6.0, track.height() + 4.0));
    p.rect_filled(groove, 2.0, Color32::BLACK);
    // the handle with grip lines
    let y = track_y(rect, fader_frac(level_db));
    let h = Rect::from_center_size(pos2(rect.center().x, y), vec2(FADER_W, HANDLE_H));
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
fn pan_knob(ui: &mut Ui, pan: i32) -> Option<i32> {
    let (rect, resp) = ui.allocate_exact_size(vec2(KNOB, KNOB), Sense::click_and_drag());
    let resp = resp.on_hover_text(format!("Pan: {}. Drag up or down, double-click: center", fmt_pan(pan)));
    let c = rect.center();
    let r = KNOB / 2.0 - 3.0;
    let p = ui.painter();
    p.circle_filled(c, r, Color32::from_rgb(0x1c, 0x1c, 0x1c));
    p.circle_stroke(c, r, Stroke::new(1.5_f32, if resp.hovered() || resp.dragged() { ORANGE } else { LINE }));
    // the top mark (center)
    p.line_segment([pos2(c.x, c.y - r - 2.0), pos2(c.x, c.y - r + 2.0)], Stroke::new(1.0_f32, DIM));
    let a = pan_angle(pan);
    let dir = vec2(a.sin(), -a.cos());
    p.line_segment([c + dir * (r * 0.25), c + dir * (r - 3.0)], Stroke::new(2.5_f32, if pan == 0 { TEXT } else { ORANGE }));
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

/// The bottom bar: playlist and song, transport, progress bar, hints.
fn transport_bar(ui: &mut Ui, player: &mut RehearsalPanel, status: Option<&Status>) {
    let loaded = status.filter(|s| s.state != PlayState::Empty);
    let total = ui.available_width();
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
        player.playlist_picker(ui, "mixer_tab_playlist", (total * 0.2).clamp(120.0, 220.0));
        player.song_picker(ui, "mixer_tab_song", (total * 0.25).clamp(140.0, 300.0), status);
        ui.separator();
        player.transport(ui, status, TRANSPORT_COMPACT);
        if let Some(st) = loaded {
            let (time, dragging) = player.time_text(st);
            ui.label(RichText::new(time).size(17.0).monospace().color(if dragging { ORANGE } else { TEXT }));
        }
    });
    ui.add_space(4.0);
    match loaded {
        Some(st) => player.progress(ui, st, (ui.available_width() - 16.0).max(100.0)),
        None => {
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width() - 16.0, 22.0), Sense::hover());
            ui.painter().rect_stroke(rect.shrink2(vec2(0.0, 4.0)), 3.0, Stroke::new(1.0_f32, LINE));
        }
    }
    ui.horizontal(|ui| {
        if let Some(line) = loaded.and_then(|st| player.next_line(st)) {
            ui.label(RichText::new(shorten(&line, 70)).size(13.0).color(ORANGE)).on_hover_text(line);
        }
        let problems = player.problems(status);
        if let Some((first, error)) = problems.first() {
            let more = if problems.len() > 1 { format!(" (+{})", problems.len() - 1) } else { String::new() };
            let all = problems.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join("\n");
            ui.label(RichText::new(format!("{}{}", shorten(first, 70), more)).size(13.0).color(if *error { RED } else { ORANGE }))
                .on_hover_text(all);
        }
    });
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
        // 0 dB is near the top, as on the original
        let z = fader_frac(0.0);
        assert!(z > 0.8 && z < 1.0, "{}", z);
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
        assert_eq!(fmt_db(0.0), "0.0 dB");
        assert_eq!(fmt_db(-12.34), "-12.3 dB");
        assert_eq!(fmt_db(f32::NEG_INFINITY), "-inf");
    }

    #[test]
    fn fader_height_uses_the_room_with_a_minimum() {
        assert_eq!(fader_height(10.0), MIN_FADER_H);
        assert_eq!(fader_height(OVERHEAD + 400.0), 400.0);
        assert_eq!(fader_height(100000.0), MAX_FADER_H);
    }

    #[test]
    fn shorten_counts_characters() {
        assert_eq!(shorten("abc", 5), "abc");
        assert_eq!(shorten("abcdefghij", 8), "abcde...");
        assert_eq!(shorten("\u{e4}\u{f6}\u{fc}\u{e4}\u{f6}\u{fc}", 5), "\u{e4}\u{f6}...");
    }
}

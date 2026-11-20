//! The rehearsal mix in the Rehearsal tab: a preset bar and 16 channel
//! strips plus the master. All the logic (maths, presets, saving) is in the
//! library (`lp16_tool::mix`, `lp16_tool::rehearsal`); this only draws and
//! edits the `MixLibrary`. The caller sends a changed mix to the engine and
//! saves the config.

use crate::gui_app::{small_btn, DIM, LINE, ORANGE, PANEL, RED, TEXT};
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, RichText, Sense, Stroke, Ui};
use lp16_tool::mix::{MixLibrary, LEVEL_MAX_DB, LEVEL_MIN_DB, PAN_MAX, STRIPS};
use lp16_tool::rehearsal::Levels;

/// width of a channel strip
const STRIP_W: f32 = 66.0;
/// height of fader and meter
const FADER_H: f32 = 130.0;
/// the meters fall by this many dB per second
const DECAY_DB_PER_S: f32 = 20.0;
/// what a meter shows for silence
const FLOOR_DB: f32 = -90.0;
/// slider position of "off", one step below the lowest level
const OFF_POS: f32 = LEVEL_MIN_DB - 1.0;

const GREEN: Color32 = Color32::from_rgb(0x6c, 0xd8, 0x6c);
const YELLOW: Color32 = Color32::from_rgb(0xe8, 0xd0, 0x4a);
const MUTE_ON: Color32 = Color32::from_rgb(0xd0, 0x40, 0x40);
const SOLO_ON: Color32 = Color32::from_rgb(0xe0, 0xc0, 0x20);

/// What the user did in the mixer during this frame.
#[derive(Default)]
pub struct MixerOutcome {
    /// values or presets changed: send the mix to the engine and save
    pub changed: bool,
    /// the clip LED was clicked
    pub reset_clip: bool,
}

#[derive(Default)]
pub struct MixerPanel {
    /// the shown meter values in dB: 16 strips, master left, master right
    shown: Option<[f32; STRIPS + 2]>,
    /// the new name while a preset is being renamed
    renaming: Option<String>,
    /// why the last preset operation was refused
    message: Option<String>,
}

/// Peak (1.0 = full scale) in dB, `FLOOR_DB` for silence.
fn peak_db(peak: f32) -> f32 {
    if peak > 1e-4 {
        20.0 * peak.log10()
    } else {
        FLOOR_DB
    }
}

/// The meter falls with `DECAY_DB_PER_S` and jumps up at once.
fn decayed(shown: f32, peak: f32, dt: f32) -> f32 {
    peak_db(peak).max(shown - DECAY_DB_PER_S * dt).max(FLOOR_DB)
}

/// Fader position (slider value) of a level; "off" is below the range.
fn fader_pos(level_db: f32) -> f32 {
    if level_db == f32::NEG_INFINITY {
        OFF_POS
    } else {
        level_db
    }
}

/// Level of a slider value, in steps of 0.1 dB; the bottom is "off".
fn fader_level(pos: f32) -> f32 {
    if pos <= LEVEL_MIN_DB - 0.5 {
        f32::NEG_INFINITY
    } else {
        ((pos * 10.0).round() / 10.0).clamp(LEVEL_MIN_DB, LEVEL_MAX_DB)
    }
}

fn fmt_level(db: f32) -> String {
    if db == f32::NEG_INFINITY {
        "off".to_string()
    } else {
        format!("{:.1}", db)
    }
}

fn fmt_pan(pan: i32) -> String {
    match pan {
        0 => "Center".to_string(),
        p if p < 0 => format!("Left {}", -p),
        p => format!("Right {}", p),
    }
}

/// Vertical position (0 bottom .. 1 top) of `db` on a meter.
fn meter_frac(db: f32) -> f32 {
    ((db - LEVEL_MIN_DB) / (LEVEL_MAX_DB - LEVEL_MIN_DB)).clamp(0.0, 1.0)
}

/// A vertical meter; `db` is the shown value.
fn meter(ui: &mut Ui, db: f32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, FADER_H), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 1.0, Color32::from_rgb(0x22, 0x22, 0x22));
    let top = rect.bottom() - rect.height() * meter_frac(db);
    // coloured by zone, so a loud meter shows green, yellow and red
    let zones = [(LEVEL_MIN_DB, -9.0, GREEN), (-9.0, 0.0, YELLOW), (0.0, LEVEL_MAX_DB, RED)];
    for (lo, hi, color) in zones {
        let y_hi = rect.bottom() - rect.height() * meter_frac(hi);
        let y_lo = rect.bottom() - rect.height() * meter_frac(lo);
        let (a, b) = (y_hi.max(top), y_lo);
        if b > a {
            p.rect_filled(egui::Rect::from_min_max(pos2(rect.left(), a), pos2(rect.right(), b)), 0.0, color);
        }
    }
    // 0 dB mark
    let y0 = rect.bottom() - rect.height() * meter_frac(0.0);
    p.line_segment([pos2(rect.left(), y0), pos2(rect.right(), y0)], Stroke::new(1.0_f32, DIM));
}

/// A slider senses only drags, so a double-click is looked up in the
/// pointer state.
fn double_clicked(ui: &Ui, resp: &egui::Response) -> bool {
    resp.hovered() && ui.input(|i| i.pointer.button_double_clicked(egui::PointerButton::Primary))
}

/// Fader (with double-click to 0 dB) and its value text. Returns the new
/// level if it changed.
fn fader(ui: &mut Ui, level_db: f32) -> Option<f32> {
    let mut pos = fader_pos(level_db);
    ui.spacing_mut().slider_width = FADER_H;
    let resp = ui.add(egui::Slider::new(&mut pos, OFF_POS..=LEVEL_MAX_DB).vertical().show_value(false).step_by(0.1));
    let resp = resp.on_hover_text("Level. Double-click: 0 dB");
    if double_clicked(ui, &resp) {
        return Some(0.0).filter(|_| level_db != 0.0);
    }
    if resp.changed() {
        let new = fader_level(pos);
        return Some(new).filter(|n| *n != level_db);
    }
    None
}

/// "M" or "S" button, in colour while on. True if clicked.
fn toggle(ui: &mut Ui, text: &str, on: bool, color: Color32) -> bool {
    let (fill, text_color) = if on { (color, Color32::BLACK) } else { (Color32::from_rgb(0x3a, 0x3a, 0x3a), TEXT) };
    ui.add(
        egui::Button::new(RichText::new(text).size(12.0).strong().color(text_color))
            .fill(fill)
            .stroke(Stroke::new(1.0_f32, if on { color } else { LINE }))
            .rounding(3.0)
            .min_size(vec2(28.0, 20.0)),
    )
    .clicked()
}

impl MixerPanel {
    pub fn ui(&mut self, ui: &mut Ui, lib: &mut MixLibrary, tracks: &[String; STRIPS], levels: &Levels) -> MixerOutcome {
        let mut out = MixerOutcome::default();
        self.preset_bar(ui, lib, &mut out);
        ui.add_space(6.0);
        // meter values: they jump up at once and fall with 20 dB/s
        let dt = ui.input(|i| i.stable_dt).min(0.25);
        let mut shown = self.shown.unwrap_or([FLOOR_DB; STRIPS + 2]);
        for (s, p) in shown.iter_mut().zip(levels.strips.iter().chain(levels.master.iter())) {
            *s = decayed(*s, *p, dt);
        }
        self.shown = Some(shown);
        // keep drawing while a meter moves or falls
        if shown.iter().any(|d| *d > FLOOR_DB) {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
        }
        let mut master_db = lib.active().master_db;
        egui::ScrollArea::horizontal().id_source("mixer_scroll").auto_shrink([false, true]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for k in 0..STRIPS {
                    let strip = lib.active().strips[k];
                    out.changed |= self.strip(ui, k, &tracks[k], &strip, shown[k], lib);
                }
                ui.add_space(8.0);
                let (changed, clip) = self.master(ui, &mut master_db, [shown[STRIPS], shown[STRIPS + 1]], levels.clip, lib);
                out.changed |= changed;
                out.reset_clip |= clip;
            });
        });
        out
    }

    fn preset_bar(&mut self, ui: &mut Ui, lib: &mut MixLibrary, out: &mut MixerOutcome) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Rehearsal mix").size(14.0).color(TEXT));
            let mut select = None;
            egui::ComboBox::from_id_source("mix_preset").width(190.0).selected_text(lib.active_name().to_string()).show_ui(
                ui,
                |ui| {
                    for (i, m) in lib.presets().iter().enumerate() {
                        if ui.selectable_label(i == lib.active_index(), &m.name).clicked() {
                            select = Some(m.name.clone());
                        }
                    }
                },
            );
            if let Some(name) = select {
                self.renaming = None;
                self.message = None;
                if lib.select(&name).is_ok() {
                    out.changed = true;
                }
            }
            if small_btn(ui, "New", true) {
                self.renaming = None;
                self.message = None;
                lib.new_mix();
                out.changed = true;
            }
            if small_btn(ui, "Copy", true) {
                self.renaming = None;
                self.message = None;
                lib.copy_active();
                out.changed = true;
            }
            if small_btn(ui, "Rename", self.renaming.is_none()) {
                self.message = None;
                self.renaming = Some(lib.active_name().to_string());
            }
            if small_btn(ui, "Delete", lib.presets().len() > 1) {
                self.renaming = None;
                match lib.delete_active() {
                    Ok(()) => {
                        self.message = None;
                        out.changed = true;
                    }
                    Err(e) => self.message = Some(e),
                }
            }
            if small_btn(ui, "Reset", true) {
                self.renaming = None;
                self.message = None;
                lib.reset_active();
                out.changed = true;
            }
        });
        if let Some(name) = self.renaming.as_mut() {
            let mut done = None;
            ui.horizontal(|ui| {
                ui.label(RichText::new("New name").size(12.5).color(DIM));
                let resp = ui.add(egui::TextEdit::singleline(name).desired_width(190.0));
                if !resp.has_focus() && !resp.lost_focus() {
                    resp.request_focus();
                }
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if small_btn(ui, "OK", true) || enter {
                    done = Some(true);
                }
                if small_btn(ui, "Cancel", true) || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    done = Some(false);
                }
            });
            match done {
                Some(true) => match lib.rename_active(name) {
                    Ok(()) => {
                        self.renaming = None;
                        self.message = None;
                        out.changed = true;
                    }
                    Err(e) => self.message = Some(e),
                },
                Some(false) => {
                    self.renaming = None;
                    self.message = None;
                }
                None => {}
            }
        }
        if let Some(m) = &self.message {
            ui.label(RichText::new(m).size(12.5).color(RED));
        }
    }

    /// One channel strip; true if something changed.
    fn strip(
        &mut self,
        ui: &mut Ui,
        k: usize,
        track: &str,
        strip: &lp16_tool::mix::Strip,
        shown_db: f32,
        lib: &mut MixLibrary,
    ) -> bool {
        let mut changed = false;
        let has_track = !track.is_empty();
        let color = if has_track { TEXT } else { DIM };
        egui::Frame::none().fill(PANEL).rounding(3.0).inner_margin(egui::Margin::same(4.0)).show(ui, |ui| {
            ui.set_width(STRIP_W - 8.0);
            ui.vertical_centered(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                ui.label(RichText::new(format!("{}", k + 1)).size(15.0).strong().color(color));
                let name = if has_track { track } else { "\u{2014}" };
                ui.add(egui::Label::new(RichText::new(name).size(11.0).color(color)).truncate(true))
                    .on_hover_text(if has_track { track.to_string() } else { "No track on this output in the song".to_string() });
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if let Some(new) = fader(ui, strip.level_db) {
                        lib.active_mut().set_level(k, new);
                        changed = true;
                    }
                    meter(ui, shown_db, 10.0);
                });
                ui.spacing_mut().slider_width = STRIP_W - 16.0;
                let mut pan = strip.pan;
                let resp = ui
                    .add(egui::Slider::new(&mut pan, -PAN_MAX..=PAN_MAX).show_value(false))
                    .on_hover_text(format!("Pan: {}. Double-click: center", fmt_pan(strip.pan)));
                if double_clicked(ui, &resp) {
                    if strip.pan != 0 {
                        lib.active_mut().set_pan(k, 0);
                        changed = true;
                    }
                } else if resp.changed() {
                    lib.active_mut().set_pan(k, pan);
                    changed = true;
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if toggle(ui, "M", strip.mute, MUTE_ON) {
                        lib.active_mut().set_mute(k, !strip.mute);
                        changed = true;
                    }
                    if toggle(ui, "S", strip.solo, SOLO_ON) {
                        lib.active_mut().set_solo(k, !strip.solo);
                        changed = true;
                    }
                });
                ui.label(RichText::new(fmt_level(strip.level_db)).size(12.0).monospace().color(color));
            });
        });
        changed
    }

    /// The master strip; returns (changed, clip LED clicked).
    fn master(
        &mut self,
        ui: &mut Ui,
        master_db: &mut f32,
        shown: [f32; 2],
        clip: bool,
        lib: &mut MixLibrary,
    ) -> (bool, bool) {
        let (mut changed, mut reset) = (false, false);
        egui::Frame::none().fill(PANEL).rounding(3.0).inner_margin(egui::Margin::same(4.0)).show(ui, |ui| {
            ui.set_width(STRIP_W + 8.0);
            ui.vertical_centered(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                ui.label(RichText::new("Master").size(13.0).strong().color(ORANGE));
                // the clip LED
                let (rect, resp) = ui.allocate_exact_size(vec2(STRIP_W + 4.0, 16.0), Sense::click());
                let on = clip;
                let p = ui.painter();
                p.circle_filled(pos2(rect.left() + 8.0, rect.center().y), 5.0, if on { RED } else { Color32::from_rgb(0x50, 0x28, 0x28) });
                p.text(
                    pos2(rect.left() + 18.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    "CLIP",
                    FontId::proportional(11.0),
                    if on { RED } else { DIM },
                );
                if resp.on_hover_text("The master was limited. Click to reset.").clicked() {
                    reset = true;
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if let Some(new) = fader(ui, *master_db) {
                        lib.active_mut().set_master(new);
                        *master_db = lib.active().master_db;
                        changed = true;
                    }
                    meter(ui, shown[0], 10.0);
                    meter(ui, shown[1], 10.0);
                });
                ui.label(RichText::new("L   R").size(10.0).color(DIM));
                ui.label(RichText::new(fmt_level(*master_db)).size(12.0).monospace().color(TEXT));
            });
        });
        (changed, reset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_jumps_up_and_falls_with_20_db_per_second() {
        let up = decayed(FLOOR_DB, 1.0, 0.03);
        assert!(up.abs() < 1e-3, "full scale = 0 dB");
        let down = decayed(0.0, 0.0, 0.5);
        assert!((down + 10.0).abs() < 1e-3, "{}", down);
        // a louder peak than the falling value takes over
        let v = decayed(-30.0, 0.1, 0.03);
        assert!((v + 20.0).abs() < 1e-3);
        assert_eq!(decayed(FLOOR_DB, 0.0, 1.0), FLOOR_DB);
    }

    #[test]
    fn fader_bottom_is_off() {
        assert_eq!(fader_pos(f32::NEG_INFINITY), OFF_POS);
        assert_eq!(fader_level(OFF_POS), f32::NEG_INFINITY);
        assert_eq!(fader_level(-60.4), -60.0);
        assert_eq!(fader_level(-60.6), f32::NEG_INFINITY);
        assert_eq!(fader_level(0.0001), 0.0);
        assert_eq!(fader_level(-6.04), -6.0);
        assert_eq!(fader_level(9.0), 6.0);
        assert_eq!(fmt_level(f32::NEG_INFINITY), "off");
        assert_eq!(fmt_level(-6.0), "-6.0");
    }
}

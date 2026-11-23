//! Parts of the mixer of the Rehearsal tab (the strips themselves are in
//! `gui_mixer_tab`): the preset bar, the meters, the S/M buttons, the
//! symbol for a song with its own mixer values. All the logic (maths, presets, saving) is in the
//! library (`lp16_tool::mix`, `lp16_tool::rehearsal`); this only draws and
//! edits the `MixLibrary`. The caller sends a changed mix to the engine and
//! saves the config.

use crate::gui_app::{small_btn, DIM, LINE, RED, TEXT};
use eframe::egui::{self, pos2, vec2, Color32, RichText, Sense, Stroke, Ui};
use lp16_tool::tr;
use lp16_tool::mix::{MixLibrary, LEVEL_MAX_DB, LEVEL_MIN_DB, STRIPS};
use lp16_tool::rehearsal::Levels;

/// the meters fall by this many dB per second
pub(crate) const DECAY_DB_PER_S: f32 = 20.0;
/// what a meter shows for silence
pub(crate) const FLOOR_DB: f32 = -90.0;

pub(crate) const GREEN: Color32 = Color32::from_rgb(0x6c, 0xd8, 0x6c);
pub(crate) const YELLOW: Color32 = Color32::from_rgb(0xe8, 0xd0, 0x4a);
pub(crate) const MUTE_ON: Color32 = Color32::from_rgb(0xd0, 0x40, 0x40);
pub(crate) const SOLO_ON: Color32 = Color32::from_rgb(0xe0, 0xc0, 0x20);

/// What the user did in the mixer during this frame.
#[derive(Default)]
pub struct MixerOutcome {
    /// values or presets changed: send the mix to the engine and save
    pub changed: bool,
    /// the clip LED was clicked
    pub reset_clip: bool,
    /// the song mix (song mode) was changed: send it and save the song
    pub song_changed: bool,
    /// "Song mix" was clicked: edit the song mix of the loaded song
    pub enter_song: bool,
    /// "Back to the rehearsal mix" was clicked
    pub leave_song: bool,
}

/// The preset bar: combo and the buttons New, Copy, Rename, Delete, Reset.
/// The header of the mixer while it edits the rehearsal mix.
#[derive(Default)]
pub struct PresetBar {
    /// the new name while a preset is being renamed
    renaming: Option<String>,
    /// why the last preset operation was refused
    message: Option<String>,
}

/// Peak (1.0 = full scale) in dB, `FLOOR_DB` for silence.
pub(crate) fn peak_db(peak: f32) -> f32 {
    if peak > 1e-4 {
        20.0 * peak.log10()
    } else {
        FLOOR_DB
    }
}

/// The meter falls with `DECAY_DB_PER_S` and jumps up at once.
pub(crate) fn decayed(shown: f32, peak: f32, dt: f32) -> f32 {
    peak_db(peak).max(shown - DECAY_DB_PER_S * dt).max(FLOOR_DB)
}

pub(crate) fn fmt_pan(pan: i32) -> String {
    match pan {
        0 => tr!("mixer.center").to_string(),
        p if p < 0 => tr!("mixer.pan_left", n = -p),
        p => tr!("mixer.pan_right", n = p),
    }
}

/// A vertical meter of `height`. `frac` maps a level to 0 (bottom) .. 1
/// (top) of the range, which is inset by `inset` at the top and bottom
/// (to align it to the dB scale of the fader).
pub(crate) fn meter_sized(ui: &mut Ui, db: f32, width: f32, height: f32, inset: f32, frac: fn(f32) -> f32) {
    let (full, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let p = ui.painter();
    p.rect_filled(full, 1.0, Color32::from_rgb(0x22, 0x22, 0x22));
    let rect = full.shrink2(vec2(0.0, inset));
    let y_of = |db: f32| rect.bottom() - rect.height() * frac(db);
    let top = y_of(db);
    // coloured by zone, so a loud meter shows green, yellow and red
    let zones = [(LEVEL_MIN_DB, -9.0, GREEN), (-9.0, 0.0, YELLOW), (0.0, LEVEL_MAX_DB, RED)];
    for (lo, hi, color) in zones {
        let (a, b) = (y_of(hi).max(top), y_of(lo));
        if b > a {
            p.rect_filled(egui::Rect::from_min_max(pos2(rect.left(), a), pos2(rect.right(), b)), 0.0, color);
        }
    }
    // 0 dB mark
    let y0 = y_of(0.0);
    p.line_segment([pos2(rect.left(), y0), pos2(rect.right(), y0)], Stroke::new(1.0_f32, DIM));
}

/// Advance the shown meter values (16 strips, master left and right) by
/// the new peaks: they jump up at once and fall with 20 dB/s. Asks for
/// repaints while a meter moves or falls.
pub(crate) fn update_shown(ui: &Ui, shown: &mut Option<[f32; STRIPS + 2]>, levels: &Levels) -> [f32; STRIPS + 2] {
    let dt = ui.input(|i| i.stable_dt).min(0.25);
    let mut now = shown.unwrap_or([FLOOR_DB; STRIPS + 2]);
    for (s, p) in now.iter_mut().zip(levels.strips.iter().chain(levels.master.iter())) {
        *s = decayed(*s, *p, dt);
    }
    *shown = Some(now);
    if now.iter().any(|d| *d > FLOOR_DB) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
    }
    now
}

/// A widget that senses only drags does not report a double-click, so it
/// is looked up in the pointer state.
pub(crate) fn double_clicked(ui: &Ui, resp: &egui::Response) -> bool {
    resp.enabled() && resp.hovered() && ui.input(|i| i.pointer.button_double_clicked(egui::PointerButton::Primary))
}

/// The symbol for "this song has its own mixer values": three small faders,
/// `size` high, centred on `c`. Drawn, since the fonts have no such sign.
pub(crate) fn paint_mixer_icon(p: &egui::Painter, c: egui::Pos2, size: f32, color: Color32) {
    let h = size / 2.0;
    let stroke = Stroke::new(1.2_f32, color);
    // the knobs at different heights, like a mix
    for (dx, knob) in [(-0.6_f32, 0.35_f32), (0.0, -0.3), (0.6, 0.15)] {
        let x = c.x + dx * h;
        p.line_segment([pos2(x, c.y - h), pos2(x, c.y + h)], stroke);
        let y = c.y + knob * h;
        p.rect_filled(egui::Rect::from_center_size(pos2(x, y), vec2(size * 0.3, size * 0.2)), 0.5, color);
    }
}

/// "M" or "S" button, in colour while on. True if clicked.
pub(crate) fn toggle(ui: &mut Ui, text: &str, on: bool, color: Color32) -> bool {
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

impl PresetBar {
    /// Draw the bar, `extra` at the end of its row; true if the mix library
    /// changed (send and save).
    pub fn ui(&mut self, ui: &mut Ui, lib: &mut MixLibrary, extra: impl FnOnce(&mut Ui)) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr!("mixer.rehearsal_mix")).size(14.0).color(TEXT));
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
                    changed = true;
                }
            }
            if small_btn(ui, tr!("mixer.new"), true) {
                self.renaming = None;
                self.message = None;
                lib.new_mix();
                changed = true;
            }
            if small_btn(ui, tr!("common.copy"), true) {
                self.renaming = None;
                self.message = None;
                lib.copy_active();
                changed = true;
            }
            if small_btn(ui, tr!("common.rename"), self.renaming.is_none()) {
                self.message = None;
                self.renaming = Some(lib.active_name().to_string());
            }
            if small_btn(ui, tr!("common.delete"), lib.presets().len() > 1) {
                self.renaming = None;
                match lib.delete_active() {
                    Ok(()) => {
                        self.message = None;
                        changed = true;
                    }
                    Err(e) => self.message = Some(e),
                }
            }
            if small_btn(ui, tr!("mixer.reset"), true) {
                self.renaming = None;
                self.message = None;
                lib.reset_active();
                changed = true;
            }
            extra(ui);
        });
        if let Some(name) = self.renaming.as_mut() {
            let mut done = None;
            ui.horizontal(|ui| {
                ui.label(RichText::new(tr!("mixer.new_name")).size(12.5).color(DIM));
                let resp = ui.add(egui::TextEdit::singleline(name).desired_width(190.0));
                if !resp.has_focus() && !resp.lost_focus() {
                    resp.request_focus();
                }
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if small_btn(ui, tr!("common.ok"), true) || enter {
                    done = Some(true);
                }
                if small_btn(ui, tr!("common.cancel"), true) || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    done = Some(false);
                }
            });
            match done {
                Some(true) => match lib.rename_active(name) {
                    Ok(()) => {
                        self.renaming = None;
                        self.message = None;
                        changed = true;
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
        changed
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
}

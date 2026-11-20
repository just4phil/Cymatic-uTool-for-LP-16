//! The "Rehearsal" tab: shows the song in the rehearsal player and controls
//! it. All the logic (mixing, transport, reading) is in the library
//! (`lp16_tool::rehearsal`, `lp16_tool::audio_out`); this only draws and
//! sends commands.

use crate::gui_app::{key, DIM, LINE, ORANGE, PANEL, RED, TEXT};
use eframe::egui::{self, vec2, Align, Color32, Key, Layout, Modifiers, RichText, Sense, Stroke, Ui};
use lp16_tool::audio_out::{Command, Player};
use lp16_tool::rehearsal::{fmt_clock, PlayState, Status};
use lp16_tool::SongProject;
use std::time::{Duration, Instant};

/// Seconds a key press or button jumps by (Shift: the long jump).
const STEP_SECONDS: f64 = 10.0;
const LONG_STEP_SECONDS: f64 = 30.0;
/// Quick jumps add up to the last target for this long, because the status
/// only shows it after the next audio block.
const SEEK_MEMORY: Duration = Duration::from_millis(400);

/// Shown after the loaded song was renamed, deleted or saved.
pub const CHANGED_NOTICE: &str = "The song was changed in the library \u{2013} load it again.";

#[derive(Default)]
pub struct RehearsalPanel {
    /// opened with the first "Play in Rehearsal"
    player: Option<Player>,
    /// why the audio output could not be opened
    device_error: Option<String>,
    /// .songprj of the loaded song
    song_path: Option<String>,
    notice: Option<String>,
    /// target time while the progress bar is being dragged
    drag_target: Option<f64>,
    /// the last target sent, and when
    last_seek: Option<(f64, Instant)>,
}

impl RehearsalPanel {
    /// Load `project` into the player and start it. Without an audio
    /// device the tab shows why instead.
    pub fn play_song(&mut self, project: &SongProject) {
        if self.player.is_none() {
            match Player::open() {
                Ok(p) => {
                    self.player = Some(p);
                    self.device_error = None;
                }
                Err(e) => self.device_error = Some(e),
            }
        }
        let Some(player) = &self.player else { return };
        player.load(project);
        player.send(Command::Play);
        self.song_path = Some(project.path.clone());
        self.notice = None;
        self.drag_target = None;
        self.last_seek = None;
    }

    /// The song file `path` is about to be renamed, deleted or saved: stop
    /// and unload it if it is the loaded one.
    pub fn song_changed(&mut self, path: &str) {
        let loaded = self.song_path.as_deref().map(|p| key(p) == key(path)).unwrap_or(false);
        if !loaded {
            return;
        }
        if let Some(player) = &self.player {
            player.send(Command::Stop);
            player.send(Command::Unload);
        }
        self.song_path = None;
        self.notice = Some(CHANGED_NOTICE.to_string());
    }

    pub fn is_playing(&self) -> bool {
        self.player.as_ref().map(|p| p.status().state == PlayState::Playing).unwrap_or(false)
    }

    /// End the audio stream and the reader thread (closing the program).
    pub fn shutdown(&mut self) {
        self.player = None;
    }

    /// Jump to `seconds` (clamped to the song) in the player.
    pub fn seek(&mut self, seconds: f64, length: f64) {
        let target = seconds.clamp(0.0, length.max(0.0));
        if let Some(player) = &self.player {
            player.send(Command::Seek(target));
        }
        self.last_seek = Some((target, Instant::now()));
    }

    /// Jump by `delta` seconds from the position the user last saw or chose.
    pub fn seek_by(&mut self, st: &Status, delta: f64) {
        let base = match self.last_seek {
            Some((t, when)) if when.elapsed() < SEEK_MEMORY => t,
            _ => st.position_seconds,
        };
        self.seek(base + delta, st.length_seconds);
    }

    /// Play or pause, as the big button does.
    pub fn toggle_play(&mut self, st: &Status) {
        let Some(player) = &self.player else { return };
        if st.state == PlayState::Playing {
            player.send(Command::Pause);
        } else if st.song_rate == st.output_rate {
            player.send(Command::Play);
        }
    }

    /// Stop: back to the start.
    pub fn stop(&mut self) {
        if let Some(player) = &self.player {
            player.send(Command::Stop);
        }
        self.last_seek = None;
    }

    /// Keyboard shortcuts; only called while the tab is shown, and nothing
    /// happens while a text field has the focus.
    fn keys(&mut self, ui: &mut Ui, st: &Status) {
        if ui.ctx().wants_keyboard_input() || st.state == PlayState::Empty {
            return;
        }
        let pressed = |ui: &Ui, m: Modifiers, k: Key| ui.input_mut(|i| i.consume_key(m, k));
        // Shift first: a plain arrow must not take the Shift arrow
        if pressed(ui, Modifiers::SHIFT, Key::ArrowLeft) {
            self.seek_by(st, -LONG_STEP_SECONDS);
        } else if pressed(ui, Modifiers::SHIFT, Key::ArrowRight) {
            self.seek_by(st, LONG_STEP_SECONDS);
        } else if pressed(ui, Modifiers::NONE, Key::ArrowLeft) {
            self.seek_by(st, -STEP_SECONDS);
        } else if pressed(ui, Modifiers::NONE, Key::ArrowRight) {
            self.seek_by(st, STEP_SECONDS);
        }
        if pressed(ui, Modifiers::NONE, Key::Space) {
            self.toggle_play(st);
        }
        if pressed(ui, Modifiers::NONE, Key::Home) {
            self.seek(0.0, st.length_seconds); // the state stays
        }
        if pressed(ui, Modifiers::NONE, Key::Escape) {
            self.stop();
        }
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        let status = self.player.as_ref().map(|p| p.status());
        let loaded = status.as_ref().map(|s| s.state != PlayState::Empty).unwrap_or(false);
        if let Some(st) = &status {
            self.keys(ui, st);
        }
        ui.with_layout(Layout::top_down(Align::Center), |ui| {
            ui.add_space(30.0);
            match (&status, loaded) {
                (Some(st), true) => self.song_view(ui, st),
                _ => self.empty_view(ui),
            }
            if let Some(e) = &self.device_error {
                ui.add_space(16.0);
                ui.add(egui::Label::new(RichText::new(e).size(15.0).color(RED)).wrap(true));
            }
            if let Some(st) = &status {
                if loaded && !st.warnings.is_empty() {
                    ui.add_space(14.0);
                    for w in st.warnings.iter() {
                        ui.add(egui::Label::new(RichText::new(format!("Warning: {}", w)).size(13.0).color(ORANGE)).wrap(true));
                    }
                } else if !loaded {
                    for w in st.warnings.iter() {
                        ui.add(egui::Label::new(RichText::new(w).size(13.0).color(ORANGE)).wrap(true));
                    }
                }
            }
        });
    }

    fn empty_view(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Rehearsal").size(30.0).color(TEXT));
        ui.add_space(16.0);
        if let Some(n) = &self.notice {
            ui.label(RichText::new(n).size(15.0).color(ORANGE));
        } else {
            ui.label(
                RichText::new("No song loaded. Right-click a song in Home and choose \"Play in Rehearsal\".")
                    .size(15.0)
                    .color(DIM),
            );
        }
    }

    fn song_view(&mut self, ui: &mut Ui, st: &Status) {
        ui.label(RichText::new(&*st.song_name).size(32.0).strong().color(TEXT));
        ui.add_space(14.0);
        // while dragging, the time shows where the jump will go
        let shown = self.drag_target.unwrap_or(st.position_seconds);
        ui.label(
            RichText::new(format!("{} / {}", fmt_clock(shown), fmt_clock(st.length_seconds)))
                .size(44.0)
                .color(if self.drag_target.is_some() { ORANGE } else { TEXT }),
        );
        ui.add_space(14.0);
        if let Some(target) = progress_bar(ui, st, &mut self.drag_target) {
            self.seek(target, st.length_seconds);
        }
        ui.add_space(18.0);
        let (mut back, mut forward, mut toggle, mut stop) = (false, false, false, false);
        ui.horizontal(|ui| {
            let w = 110.0;
            ui.add_space(((ui.available_width() - 4.0 * w - 3.0 * ui.spacing().item_spacing.x) / 2.0).max(0.0));
            // a song of another sample rate cannot be played
            let can_play = st.song_rate == st.output_rate;
            back = big_button(ui, "\u{2212}10 s", true, w);
            toggle = big_button(ui, if st.state == PlayState::Playing { "Pause" } else { "Play" }, can_play, w);
            stop = big_button(ui, "Stop", true, w);
            forward = big_button(ui, "+10 s", true, w);
        });
        if back {
            self.seek_by(st, -STEP_SECONDS);
        }
        if forward {
            self.seek_by(st, STEP_SECONDS);
        }
        if toggle {
            self.toggle_play(st);
        }
        if stop {
            self.stop();
        }
        ui.add_space(10.0);
        ui.label(
            RichText::new(
                "Space: play/pause   \u{2190} \u{2192}: \u{b1}10 s   Shift + \u{2190} \u{2192}: \u{b1}30 s   Home: to start   Esc: stop",
            )
            .size(13.0)
            .color(DIM),
        );
        ui.add_space(14.0);
        let Some(player) = &self.player else { return };
        if st.underruns > 0 {
            ui.label(
                RichText::new(format!("Dropouts: {} (the audio buffer ran empty)", st.underruns))
                    .size(14.0)
                    .color(RED),
            );
        }
        ui.label(
            RichText::new(format!(
                "Output: {}, {} Hz \u{2013} song: {} Hz",
                player.device_name(),
                st.output_rate,
                st.song_rate
            ))
            .size(12.0)
            .color(DIM),
        );
    }
}

fn big_button(ui: &mut Ui, text: &str, enabled: bool, width: f32) -> bool {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).size(22.0).color(TEXT))
            .fill(Color32::from_rgb(0x3a, 0x3a, 0x3a))
            .stroke(Stroke::new(1.5_f32, ORANGE))
            .rounding(4.0)
            .min_size(vec2(width, 44.0)),
    )
    .clicked()
}

/// Position in the song. Click or drag to jump: while dragging `drag`
/// holds the target (seconds), the jump is returned when the mouse is
/// released; a click returns it at once.
fn progress_bar(ui: &mut Ui, st: &Status, drag: &mut Option<f64>) -> Option<f64> {
    let w = (ui.available_width() - 60.0).clamp(100.0, 900.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 22.0), Sense::click_and_drag());
    let target_at = |x: f32| ((x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64 * st.length_seconds;
    let mut jump = None;
    if resp.dragged() {
        if let Some(pos) = resp.interact_pointer_pos() {
            *drag = Some(target_at(pos.x));
        }
    }
    if resp.drag_stopped() {
        jump = drag.take();
    } else if resp.clicked() {
        jump = resp.interact_pointer_pos().map(|p| target_at(p.x));
        *drag = None;
    }
    let shown = drag.unwrap_or(st.position_seconds);
    let frac = if st.length_seconds <= 0.0 { 0.0 } else { (shown / st.length_seconds).clamp(0.0, 1.0) as f32 };
    let bar = rect.shrink2(vec2(0.0, 4.0));
    let p = ui.painter();
    p.rect_filled(bar, 3.0, PANEL);
    p.rect_stroke(bar, 3.0, Stroke::new(1.0_f32, LINE));
    let mut fill = bar;
    fill.set_width(bar.width() * frac);
    p.rect_filled(fill, 3.0, ORANGE);
    if resp.hovered() || drag.is_some() {
        let x = bar.left() + bar.width() * frac;
        p.circle_filled(egui::pos2(x, rect.center().y), 8.0, TEXT);
    }
    jump
}

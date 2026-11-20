//! The "Rehearsal" tab: shows the song in the rehearsal player and controls
//! it. All the logic (mixing, transport, reading) is in the library
//! (`lp16_tool::rehearsal`, `lp16_tool::audio_out`); this only draws and
//! sends commands.

use crate::gui_app::{key, DIM, LINE, ORANGE, PANEL, RED, TEXT};
use eframe::egui::{self, vec2, Align, Color32, Layout, RichText, Sense, Stroke, Ui};
use lp16_tool::audio_out::{Command, Player};
use lp16_tool::rehearsal::{fmt_clock, PlayState, Status};
use lp16_tool::SongProject;

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

    pub fn ui(&mut self, ui: &mut Ui) {
        let status = self.player.as_ref().map(|p| p.status());
        let loaded = status.as_ref().map(|s| s.state != PlayState::Empty).unwrap_or(false);
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
        ui.label(
            RichText::new(format!("{} / {}", fmt_clock(st.position_seconds), fmt_clock(st.length_seconds)))
                .size(44.0)
                .color(TEXT),
        );
        ui.add_space(14.0);
        progress_bar(ui, st);
        ui.add_space(18.0);
        let Some(player) = &self.player else { return };
        ui.horizontal(|ui| {
            let w = 110.0;
            ui.add_space(((ui.available_width() - 2.0 * w - 14.0) / 2.0).max(0.0));
            let playing = st.state == PlayState::Playing;
            // a song of another sample rate cannot be played
            let can_play = st.song_rate == st.output_rate;
            if big_button(ui, if playing { "Pause" } else { "Play" }, can_play, w) {
                player.send(if playing { Command::Pause } else { Command::Play });
            }
            if big_button(ui, "Stop", true, w) {
                player.send(Command::Stop);
            }
        });
        ui.add_space(14.0);
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

/// position in the song; display only
fn progress_bar(ui: &mut Ui, st: &Status) {
    let w = (ui.available_width() - 60.0).clamp(100.0, 900.0);
    let (rect, _) = ui.allocate_exact_size(vec2(w, 14.0), Sense::hover());
    let frac = if st.length_frames == 0 { 0.0 } else { (st.position_frames as f32 / st.length_frames as f32).clamp(0.0, 1.0) };
    let p = ui.painter();
    p.rect_filled(rect, 3.0, PANEL);
    p.rect_stroke(rect, 3.0, Stroke::new(1.0_f32, LINE));
    let mut fill = rect;
    fill.set_width(rect.width() * frac);
    p.rect_filled(fill, 3.0, ORANGE);
}

//! The "Rehearsal" tab: the playlist and song list on the left, the song in
//! the rehearsal player in the middle, the mixer below it. The list and the
//! mixer can be dragged wider and taller; the strips scale with the mixer's
//! frame. The mixer edits the rehearsal mix or, in song mode, the song mix of
//! the loaded song (its uTool2 mixer values, saved in its .songprj). All the
//! logic (the queue, the stream, the song mix) is in the session
//! (`lp16_tool::rehearsal_session`); this only draws and calls it.

use crate::gui_app::{DIM, LINE, ORANGE, PANEL, RED, TEXT};
use crate::gui_mixer::MixerOutcome;
use crate::gui_mixer_tab::{ConsoleMode, MixerConsole};
use crate::gui_playlist;
use eframe::egui::{self, vec2, Align, Color32, Key, Layout, Margin, Modifiers, RichText, Sense, Stroke, Ui};
use lp16_tool::tr;
use lp16_tool::mix::{song_strips, MixLibrary, SongStrip, STRIPS};
use lp16_tool::rehearsal::{fmt_clock, Levels, PlayState, Status};
use lp16_tool::rehearsal_queue::next_caption;
use lp16_tool::rehearsal_session::{Session, SONG_SAVE_DELAY};
use lp16_tool::setlist::SetMarkers;
use std::collections::HashMap;
use std::time::Duration;

/// Seconds a key press or button jumps by (Shift: the long jump).
const STEP_SECONDS: f64 = 10.0;
const LONG_STEP_SECONDS: f64 = 30.0;
/// Width of the left part (playlist and song list) at the start, and the
/// narrowest it can be dragged; at most it takes this share of the tab.
const LIST_WIDTH: f32 = 290.0;
const MIN_LIST_WIDTH: f32 = 160.0;
const MAX_LIST_SHARE: f32 = 0.6;
/// Share of the height the mixer gets at the start, the lowest it can be
/// dragged, and the room it always leaves to the player above it.
const MIXER_SHARE: f32 = 0.6;
const MIN_MIXER_HEIGHT: f32 = 120.0;
const MIN_PLAYER_HEIGHT: f32 = 120.0;
/// How long the MIDI indicator stays lit after a message.
const MIDI_FLASH: Duration = Duration::from_millis(150);

#[derive(Default)]
pub struct RehearsalPanel {
    /// the player: queue, stream, mix, song mix
    pub session: Session,
    /// target time while the progress bar is being dragged
    drag_target: Option<f64>,
    /// the rehearsal mix (below the player)
    mixer: MixerConsole,
    /// the autoplay checkbox was changed (see `take_autoplay_change`)
    autoplay_changed: bool,
    /// encore / end of concert markers of the playlists, by name
    markers: HashMap<String, SetMarkers>,
}

/// The range the list can be dragged in, for a tab `total` wide.
fn list_width_range(total: f32) -> std::ops::RangeInclusive<f32> {
    MIN_LIST_WIDTH..=(total * MAX_LIST_SHARE).max(MIN_LIST_WIDTH)
}

/// The range the mixer can be dragged in, for a tab `total` high.
fn mixer_height_range(total: f32) -> std::ops::RangeInclusive<f32> {
    MIN_MIXER_HEIGHT..=(total - MIN_PLAYER_HEIGHT).max(MIN_MIXER_HEIGHT)
}

/// The skipped songs below the list.
fn list_warnings(ui: &mut Ui, status: Option<&Status>) {
    if let Some(st) = status {
        for w in st.queue_warnings.iter() {
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(tr!("common.warning", text = w)).size(12.0).color(ORANGE)).wrap(true));
        }
    }
}

impl RehearsalPanel {
    /// The new value if the user changed the autoplay checkbox since the
    /// last call, so the owner saves the config.
    pub fn take_autoplay_change(&mut self) -> Option<bool> {
        std::mem::take(&mut self.autoplay_changed).then_some(self.session.autoplay())
    }

    /// The checkbox "Autoplay after 5 seconds"; the same one in every tab.
    pub fn autoplay_option(&mut self, ui: &mut Ui) {
        let mut on = self.session.autoplay();
        let resp = ui.checkbox(&mut on, tr!("rehearsal.autoplay")).on_hover_text(
            tr!("rehearsal.autoplay_tip"),
        );
        if resp.changed() {
            self.session.set_autoplay(on);
            self.autoplay_changed = true;
        }
    }

    /// Called every frame, whichever tab is shown: saves a changed song mix
    /// once the user paused, and asks for the frame that does it.
    pub fn tick(&mut self, ctx: &egui::Context) {
        if self.session.tick() {
            ctx.request_repaint_after(SONG_SAVE_DELAY);
        }
    }

    /// Keyboard shortcuts; only called while the tab is shown, and nothing
    /// happens while a text field has the focus.
    pub fn keys(&mut self, ui: &mut Ui, st: &Status) {
        if ui.ctx().wants_keyboard_input() || st.state == PlayState::Empty {
            return;
        }
        let s = &mut self.session;
        let pressed = |ui: &Ui, m: Modifiers, k: Key| ui.input_mut(|i| i.consume_key(m, k));
        // Shift first: a plain arrow must not take the Shift arrow
        if pressed(ui, Modifiers::SHIFT, Key::ArrowLeft) {
            s.seek_by(st, -LONG_STEP_SECONDS);
        } else if pressed(ui, Modifiers::SHIFT, Key::ArrowRight) {
            s.seek_by(st, LONG_STEP_SECONDS);
        } else if pressed(ui, Modifiers::NONE, Key::ArrowLeft) {
            s.seek_by(st, -STEP_SECONDS);
        } else if pressed(ui, Modifiers::NONE, Key::ArrowRight) {
            s.seek_by(st, STEP_SECONDS);
        }
        if pressed(ui, Modifiers::NONE, Key::ArrowUp) {
            s.move_mark(false);
        } else if pressed(ui, Modifiers::NONE, Key::ArrowDown) {
            s.move_mark(true);
        }
        if pressed(ui, Modifiers::NONE, Key::Space) {
            s.toggle_play(st);
        }
        if pressed(ui, Modifiers::NONE, Key::Home) {
            s.seek(0.0, st.length_seconds); // the state stays
        }
        if pressed(ui, Modifiers::NONE, Key::Escape) {
            s.stop();
        }
    }

    // -- drawing ---------------------------------------------------------------

    /// Draw the tab: the song list on the left, the player, below it the
    /// rehearsal mix `mixes`. The list and the mixer are panels the user can
    /// drag wider and taller (egui memory keeps their size; eframe saves it
    /// and restores it at the next start).
    /// Returns true if the mix (values or presets) was changed, so the
    /// caller saves the config.
    pub fn ui(&mut self, ui: &mut Ui, mixes: &mut MixLibrary) -> bool {
        let status = self.begin_frame(ui.ctx());
        let levels = self.session.take_levels();
        let (total_w, total_h) = (ui.available_width(), ui.available_height());
        egui::SidePanel::left("rehearsal_list_panel")
            .resizable(true)
            .default_width(LIST_WIDTH)
            .width_range(list_width_range(total_w))
            .frame(egui::Frame::none().inner_margin(Margin { right: 6.0, ..Default::default() }))
            .show_inside(ui, |ui| self.list_side(ui, status.as_ref()));
        let mut out = MixerOutcome::default();
        egui::TopBottomPanel::bottom("rehearsal_mixer_panel")
            .resizable(true)
            .default_height((total_h * MIXER_SHARE).max(MIN_MIXER_HEIGHT))
            .height_range(mixer_height_range(total_h))
            .frame(egui::Frame::none().inner_margin(Margin { left: 6.0, top: 8.0, ..Default::default() }))
            .show_inside(ui, |ui| {
                let loaded = status.as_ref().filter(|s| s.state != PlayState::Empty).and_then(|s| s.index);
                let names = self.session.track_names().clone();
                let song = loaded.and_then(|i| self.session.project(i));
                let song_mix = song.map(song_strips).unwrap_or([SongStrip::default(); STRIPS]);
                let song_name = song.map(|p| p.name.clone());
                let mode = match self.session.song_mix() {
                    Some(edit) => ConsoleMode::Song {
                        name: edit.name,
                        strips: edit.strips,
                        used: edit.used,
                        error: edit.error,
                    },
                    None => ConsoleMode::Rehearsal { song: song_mix, song_name: song_name.as_deref() },
                };
                out = self.mixer.ui(ui, "rehearsal_mixer_scroll", mixes, mode, &names, &levels);
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::none().inner_margin(Margin { left: 6.0, ..Default::default() }))
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().id_source("rehearsal_player_scroll").auto_shrink([false, false]).show(
                    ui,
                    |ui| {
                        ui.with_layout(Layout::top_down(Align::Center), |ui| self.player_area(ui, status.as_ref()));
                    },
                );
            });
        if out.changed {
            self.session.set_mix(mixes.active().params());
        }
        if out.reset_clip {
            self.session.reset_clip();
        }
        if out.song_changed {
            self.session.song_mix_changed();
        }
        if out.leave_song {
            self.session.leave_song_mix();
        }
        if out.enter_song {
            self.session.edit_song_mix();
            ui.ctx().request_repaint();
        }
        out.changed
    }

    /// The left panel: autoplay, the playlist and the song list (it scrolls
    /// by itself), the skipped songs at the bottom.
    fn list_side(&mut self, ui: &mut Ui, status: Option<&Status>) {
        self.autoplay_option(ui);
        ui.add_space(4.0);
        if status.map(|s| !s.queue_warnings.is_empty()).unwrap_or(false) {
            egui::TopBottomPanel::bottom("rehearsal_list_warnings")
                .frame(egui::Frame::none())
                .show_separator_line(false)
                .show_inside(ui, |ui| list_warnings(ui, status));
        }
        egui::ScrollArea::vertical()
            .id_source("rehearsal_list_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| self.list_area(ui, status));
    }

    /// The work every tab that shows the player does first in each frame:
    /// the session looks after the stream; repaints are asked for while
    /// something moves. Returns the status to draw.
    pub fn begin_frame(&mut self, ctx: &egui::Context) -> Option<Status> {
        let status = self.session.poll();
        if let Some(st) = &status {
            if st.state == PlayState::Countdown {
                ctx.request_repaint_after(Duration::from_millis(100));
            } else if st.state != PlayState::Empty && st.length_frames == 0 {
                // the song is still being opened
                ctx.request_repaint_after(Duration::from_millis(30));
            }
        }
        status
    }

    /// The meter peaks since the last call (only the tab that is shown
    /// calls this, once per frame).
    pub fn take_levels(&self) -> Levels {
        self.session.take_levels()
    }

    /// The markers of the playlists of the library (called whenever the
    /// library is read again).
    pub fn set_markers(&mut self, markers: Vec<(String, SetMarkers)>) {
        self.markers = markers.into_iter().collect();
    }

    /// Playlist combo and song list.
    fn list_area(&mut self, ui: &mut Ui, status: Option<&Status>) {
        let rows = self.session.rows(status);
        let names = self.session.playlist_names();
        let single = self.session.is_single();
        let has_songs = self.session.has_library_songs();
        let loaded = self.session.loaded_playlist();
        // single songs have no markers
        let markers = loaded.and_then(|n| self.markers.get(n)).copied().unwrap_or_default().clamp(rows.len());
        let out = gui_playlist::ui(ui, &names, loaded, single, has_songs, &rows, markers);
        if let Some(name) = out.select {
            self.session.play_playlist(&name);
        }
        if out.library {
            self.session.play_library();
        }
        if let Some(i) = out.click {
            self.session.click_song(i);
        }
        if let Some(i) = out.play {
            self.session.play_index(i);
        }
    }

    /// "pos / length" for the clock; while the progress bar is dragged it
    /// shows the target (second value true).
    pub fn time_text(&self, st: &Status) -> (String, bool) {
        let shown = self.drag_target.unwrap_or(st.position_seconds);
        (format!("{} / {}", fmt_clock(shown), fmt_clock(st.length_seconds)), self.drag_target.is_some())
    }

    /// The progress bar: click or drag to jump.
    pub fn progress(&mut self, ui: &mut Ui, st: &Status, width: f32) {
        if let Some(target) = progress_bar(ui, st, &mut self.drag_target, width) {
            self.session.seek(target, st.length_seconds);
        }
    }

    /// The transport buttons: previous song, -10 s, Play/Pause, Stop,
    /// +10 s, next song. Without a loaded song they are disabled, only
    /// Play starts the queue.
    pub fn transport(&mut self, ui: &mut Ui, status: Option<&Status>, style: TransportStyle) {
        let st = status.filter(|s| s.state != PlayState::Empty);
        let (mut prev, mut next) = (false, false);
        let (mut back, mut forward, mut toggle, mut stop) = (false, false, false, false);
        let can_start = st.is_none() && self.session.can_start();
        ui.horizontal(|ui| {
            let (w, small) = (style.wide, style.small);
            if style.center {
                ui.add_space(((ui.available_width() - 4.0 * w - 2.0 * small - 5.0 * ui.spacing().item_spacing.x) / 2.0).max(0.0));
            }
            let btn = |ui: &mut Ui, text: &str, enabled: bool, width: f32| {
                sized_button(ui, text, enabled, width, style.font, style.height)
            };
            // a song of another sample rate cannot be played
            let can_play = st.map(|s| s.song_rate == s.output_rate).unwrap_or(can_start);
            let has_prev = st.map(|s| s.index.map(|i| i > 0).unwrap_or(false) || s.position_seconds > 0.0).unwrap_or(false);
            let has_next = st.map(|s| s.next_index.is_some()).unwrap_or(false);
            let playing = st.map(|s| s.state == PlayState::Playing).unwrap_or(false);
            prev = btn(ui, "\u{23ee}", has_prev, small).on_hover_text(tr!("rehearsal.prev")).clicked();
            back = btn(ui, "\u{2212}10 s", st.is_some(), w).clicked();
            let (icon, tip) = if playing { (Icon::Pause, tr!("rehearsal.pause")) } else { (Icon::Play, tr!("rehearsal.play")) };
            toggle = icon_button(ui, icon, can_play, w, style.height).on_hover_text(tip).clicked();
            stop = icon_button(ui, Icon::Stop, st.is_some(), w, style.height).on_hover_text(tr!("rehearsal.stop")).clicked();
            forward = btn(ui, "+10 s", st.is_some(), w).clicked();
            next = btn(ui, "\u{23ed}", has_next, small).on_hover_text(tr!("rehearsal.next")).clicked();
        });
        if prev {
            self.session.prev_song();
        }
        if next {
            self.session.next_song();
        }
        if let Some(st) = st {
            if back {
                self.session.seek_by(st, -STEP_SECONDS);
            }
            if forward {
                self.session.seek_by(st, STEP_SECONDS);
            }
            if toggle {
                self.session.toggle_play(st);
            }
            if stop {
                self.session.stop();
            }
        } else if toggle {
            self.session.play_first();
        }
    }

    fn player_area(&mut self, ui: &mut Ui, status: Option<&Status>) {
        let loaded = status.map(|s| s.state != PlayState::Empty).unwrap_or(false);
        if let Some(st) = status {
            self.keys(ui, st);
        }
        ui.add_space(12.0);
        if let Some(n) = self.session.notice() {
            ui.add(egui::Label::new(RichText::new(n).size(15.0).color(ORANGE)).wrap(true));
            ui.add_space(10.0);
        }
        match (status, loaded) {
            (Some(st), true) => self.song_view(ui, st),
            _ => self.empty_view(ui),
        }
        if let Some(e) = self.session.error() {
            ui.add_space(16.0);
            ui.add(egui::Label::new(RichText::new(e).size(15.0).color(RED)).wrap(true));
        }
        if let Some(st) = status {
            if loaded && !st.warnings.is_empty() {
                ui.add_space(14.0);
                for w in st.warnings.iter() {
                    ui.add(egui::Label::new(RichText::new(tr!("common.warning", text = w)).size(13.0).color(ORANGE)).wrap(true));
                }
            } else if !loaded {
                for w in st.warnings.iter() {
                    ui.add(egui::Label::new(RichText::new(w).size(13.0).color(ORANGE)).wrap(true));
                }
            }
        }
    }

    fn empty_view(&mut self, ui: &mut Ui) {
        ui.label(RichText::new(tr!("tab.rehearsal")).size(30.0).color(TEXT));
        ui.add_space(16.0);
        let has_queue = self.session.has_queue();
        if has_queue && !self.session.can_start() {
            ui.add(
                egui::Label::new(RichText::new(tr!("rehearsal.none_playable")).size(15.0).color(RED))
                    .wrap(true),
            );
        } else if has_queue {
            // a playlist is chosen, nothing is loaded yet
            if icon_button(ui, Icon::Play, true, 110.0, 44.0).on_hover_text(tr!("rehearsal.play")).clicked() {
                self.session.play_first();
            }
        } else if self.session.notice().is_none() {
            ui.add(
                egui::Label::new(
                    RichText::new(
                        tr!("rehearsal.no_song", action = tr!("common.play_in_rehearsal")),
                    )
                    .size(15.0)
                    .color(DIM),
                )
                .wrap(true),
            );
        }
    }

    fn song_view(&mut self, ui: &mut Ui, st: &Status) {
        if let Some(caption) = next_caption(st.state, &st.song_name, st.countdown_seconds) {
            ui.add(egui::Label::new(RichText::new(caption).size(28.0).strong().color(ORANGE)).wrap(true));
            ui.add_space(10.0);
        }
        ui.add(egui::Label::new(RichText::new(&*st.song_name).size(32.0).strong().color(TEXT)).wrap(true));
        ui.add_space(14.0);
        // while dragging, the time shows where the jump will go
        let (time, dragging) = self.time_text(st);
        ui.label(RichText::new(time).size(44.0).color(if dragging { ORANGE } else { TEXT }));
        ui.add_space(14.0);
        let width = (ui.available_width() - 60.0).clamp(100.0, 900.0);
        self.progress(ui, st, width);
        ui.add_space(18.0);
        self.transport(ui, Some(st), TRANSPORT_BIG);
        ui.add_space(10.0);
        ui.add(
            egui::Label::new(
                RichText::new(
                    "Space: play/pause   \u{2190} \u{2192}: \u{b1}10 s   Shift + \u{2190} \u{2192}: \u{b1}30 s   \
                     \u{2191} \u{2193}: select song   Home: to start   Esc: stop",
                )
                .size(13.0)
                .color(DIM),
            )
            .wrap(true),
        );
        ui.add_space(14.0);
        if st.underruns > 0 {
            ui.label(
                RichText::new(tr!("rehearsal.dropouts", n = st.underruns))
                    .size(14.0)
                    .color(RED),
            );
        }
        let output = match self.session.stream_info() {
            Some(info) => {
                tr!("rehearsal.output", device = info.device, rate = st.output_rate, song = st.song_rate)
            }
            None => tr!("rehearsal.no_output", song = st.song_rate),
        };
        ui.label(RichText::new(output).size(12.0).color(DIM));
        self.midi_line(ui, st);
    }

    /// "MIDI: <Port> - <n> events" with an indicator that lights up for
    /// 150 ms at every message; "MIDI: no file" / "MIDI: off" otherwise.
    fn midi_line(&self, ui: &mut Ui, st: &Status) {
        let (text, lit) = midi_summary(self.session.midi_port(), st);
        ui.horizontal(|ui| {
            ui.add_space(((ui.available_width() - 260.0) / 2.0).max(0.0));
            let (rect, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
            let color = if lit.is_some() { ORANGE } else { LINE };
            ui.painter().circle_filled(rect.center(), 5.0, color);
            ui.label(RichText::new(text).size(12.0).color(DIM));
        });
        if let Some(left) = lit {
            ui.ctx().request_repaint_after(left);
        }
        if !self.session.midi_port().is_empty() {
            if let Some(e) = self.session.midi_status().error {
                ui.add(egui::Label::new(RichText::new(e).size(13.0).color(ORANGE)).wrap(true));
            }
        }
    }
}

/// The MIDI line of the status, and how much longer the indicator is lit.
fn midi_summary(port: &str, st: &Status) -> (String, Option<Duration>) {
    if port.is_empty() {
        return (tr!("rehearsal.midi_off").to_string(), None);
    }
    if !st.midi.has_file {
        return (tr!("rehearsal.midi_no_file").to_string(), None);
    }
    let lit = st.midi.last_event.and_then(|t| MIDI_FLASH.checked_sub(t.elapsed())).filter(|d| !d.is_zero());
    (tr!("rehearsal.midi_events", port = port, n = st.midi.events_sent), lit)
}

fn sized_button(ui: &mut Ui, text: &str, enabled: bool, width: f32, font: f32, height: f32) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).size(font).color(TEXT))
            .fill(Color32::from_rgb(0x3a, 0x3a, 0x3a))
            .stroke(Stroke::new(1.5_f32, ORANGE))
            .rounding(4.0)
            .min_size(vec2(width, height)),
    )
}

/// The symbols of the transport buttons, drawn (not taken from the font).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Icon {
    Play,
    Pause,
    Stop,
}

/// A transport button with `icon`; the symbol is scaled to the height.
fn icon_button(ui: &mut Ui, icon: Icon, enabled: bool, width: f32, height: f32) -> egui::Response {
    let resp = sized_button(ui, "", enabled, width, 12.0, height);
    let color = if enabled { TEXT } else { DIM };
    let c = resp.rect.center();
    let s = height * 0.42; // size of the symbol
    let p = ui.painter();
    match icon {
        Icon::Play => {
            // a triangle pointing right, centred by its area
            let pts = vec![
                egui::pos2(c.x - s * 0.4, c.y - s / 2.0),
                egui::pos2(c.x + s * 0.55, c.y),
                egui::pos2(c.x - s * 0.4, c.y + s / 2.0),
            ];
            p.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
        }
        Icon::Pause => {
            let bar = vec2(s * 0.3, s);
            for dx in [-s * 0.27, s * 0.27] {
                p.rect_filled(egui::Rect::from_center_size(egui::pos2(c.x + dx, c.y), bar), 1.0, color);
            }
        }
        Icon::Stop => {
            p.rect_filled(egui::Rect::from_center_size(c, vec2(s * 0.85, s * 0.85)), 1.5, color);
        }
    }
    resp
}

/// Size of the transport buttons.
#[derive(Clone, Copy)]
pub struct TransportStyle {
    pub font: f32,
    pub height: f32,
    /// width of -10 s, Play/Pause, Stop and +10 s
    pub wide: f32,
    /// width of previous and next song
    pub small: f32,
    /// centre the row
    pub center: bool,
}

/// The small buttons of the preview player in the "Export Stereo-MP3" tab.
pub const TRANSPORT_SMALL: TransportStyle = TransportStyle { font: 14.0, height: 28.0, wide: 62.0, small: 38.0, center: false };

/// The big buttons of the Rehearsal tab.
pub const TRANSPORT_BIG: TransportStyle = TransportStyle { font: 22.0, height: 44.0, wide: 100.0, small: 56.0, center: true };

/// Position in the song. Click or drag to jump: while dragging `drag`
/// holds the target (seconds), the jump is returned when the mouse is
/// released; a click returns it at once.
fn progress_bar(ui: &mut Ui, st: &Status, drag: &mut Option<f64>, w: f32) -> Option<f64> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use lp16_tool::rehearsal_queue::SongRow;

    fn rows(n: usize) -> Vec<SongRow> {
        (1..=n)
            .map(|i| SongRow {
                number: i,
                name: format!("Song {}", i),
                length: "3:00".into(),
                midi: false,
                mixer: false,
                error: None,
                current: false,
                marked: i == 1,
                next: false,
            })
            .collect()
    }

    /// The height the list's scroll area shows for `n` songs in the left
    /// panel of a window `height` high, laid out like `RehearsalPanel::ui`;
    /// and the height its content needs.
    fn shown_height(n: usize, height: f32) -> (f32, f32) {
        let ctx = egui::Context::default();
        let mut result = (0.0, 0.0);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), vec2(1400.0, height))),
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                egui::SidePanel::left("list").default_width(LIST_WIDTH).show_inside(ui, |ui| {
                    let out = egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        gui_playlist::ui(ui, &[], None, false, false, &rows(n), Default::default());
                    });
                    result = (out.inner_rect.height(), out.content_size.y);
                });
            });
        });
        result
    }

    #[test]
    fn the_song_list_uses_the_height_of_the_window() {
        let (shown, needed) = shown_height(12, 900.0);
        assert!(needed > 12.0 * 26.0, "{}", needed);
        assert!(shown + 0.5 >= needed, "all 12 songs are visible: {} of {}", shown, needed);
        // a low window: the list scrolls, but fills the panel
        let (shown, needed) = shown_height(40, 500.0);
        assert!(shown < needed && shown > 400.0, "{} of {}", shown, needed);
    }

    #[test]
    fn the_panels_can_be_dragged_within_limits() {
        assert_eq!(list_width_range(1000.0), MIN_LIST_WIDTH..=600.0);
        assert_eq!(mixer_height_range(900.0), MIN_MIXER_HEIGHT..=780.0);
        // a tiny window never gives an empty range
        assert_eq!(list_width_range(10.0), MIN_LIST_WIDTH..=MIN_LIST_WIDTH);
        assert_eq!(mixer_height_range(10.0), MIN_MIXER_HEIGHT..=MIN_MIXER_HEIGHT);
    }

    #[test]
    fn the_tab_draws_in_small_and_large_windows() {
        for (w, h) in [(300.0, 200.0), (1200.0, 800.0), (2600.0, 1400.0)] {
            let ctx = egui::Context::default();
            let mut panel = RehearsalPanel::default();
            let mut mixes = MixLibrary::default();
            for _ in 0..2 {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), vec2(w, h))),
                    ..Default::default()
                };
                let _ = ctx.run(input, |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        panel.ui(ui, &mut mixes);
                    });
                });
            }
        }
    }
}

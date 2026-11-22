//! The "Export Stereo-MP3" tab: the backup MP3s for the case that the LP-16
//! fails. One MP3 per song of a playlist, left and right are two mono mixes
//! of the 16 outputs (typically backings left, click right), each with its
//! own mixer. All the logic is in the library (`lp16_tool::backup_mix`,
//! `lp16_tool::backup`); this draws, edits the settings and runs the export
//! in a background thread.
//!
//! The preview player at the top is the player of the Rehearsal tab (one
//! audio stream, one engine): while this tab is shown the engine mixes with
//! the backup mix (`RehearsalPanel::set_stereo`, see `preview_gains`)
//! instead of the rehearsal mix, so what is heard is what the MP3 gets.

use crate::gui_app::{folder_btn, small_btn, DIM, ORANGE, PANEL, RED, TEXT};
use crate::gui_mixer::{update_shown, FLOOR_DB, GREEN};
use crate::gui_mixer_tab::backup_side;
use crate::gui_playlist::playlist_combo;
use crate::gui_rehearsal::{RehearsalPanel, TRANSPORT_SMALL};
use lp16_tool::rehearsal_session::track_names;
use eframe::egui::{self, vec2, Align2, Color32, FontId, RichText, Sense, Stroke, Ui};
use lp16_tool::backup::{render_song, Tags};
use lp16_tool::backup_mix::{file_name, unique_names, BackupMix, BackupSettings};
use lp16_tool::mix::STRIPS;
use lp16_tool::rehearsal::{fmt_clock, PlayState, Status, StereoGains};
use lp16_tool::{PlaylistSong, SongProject};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Fill of the strips that reach a side.
const LEFT_FILL: Color32 = Color32::from_rgb(0x28, 0x3c, 0x4a);
const RIGHT_FILL: Color32 = Color32::from_rgb(0x4a, 0x36, 0x28);
/// Height of the report below the settings.
const REPORT_H: f32 = 150.0;

/// One line of the report.
struct Line {
    /// file name (or song name, if it failed before)
    name: String,
    ok: bool,
    /// duration, or the error
    text: String,
    /// orange notes: tracks left out, clipping, ...
    notes: Vec<String>,
}

/// The running (or finished) export.
struct Job {
    total: usize,
    done: usize,
    current: String,
    /// fraction of the current song
    frac: f64,
    lines: Vec<Line>,
    finished: bool,
    cancel: bool,
    started: Instant,
    elapsed: f64,
    dir: String,
}

#[derive(Default)]
pub struct BackupPanel {
    audio_lib: String,
    library: Vec<(String, Vec<PlaylistSong>)>,
    playlist: Option<String>,
    /// track names per output, for the playlist in `names_for`
    names: [String; STRIPS],
    names_for: Option<String>,
    /// name of the first song of the playlist, for the file name example
    first_song: String,
    job: Option<Arc<Mutex<Job>>>,
    message: Option<String>,
    /// what the preview plays: both sides, or one side on both speakers
    monitor: Monitor,
    /// the song chosen in the preview while another playlist is loaded
    preview_index: usize,
    /// the shown meter values (16 strips, master left, master right)
    shown: Option<[f32; STRIPS + 2]>,
}

/// What the preview player plays.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Monitor {
    /// left side left, right side right, as in the MP3
    #[default]
    Stereo,
    /// only the left side, on both speakers
    Left,
    /// only the right side, on both speakers
    Right,
}

/// The gains of the preview for `mix` and `monitor`: the gains of the MP3,
/// or one side on both outputs.
pub fn preview_gains(mix: &BackupMix, monitor: Monitor) -> StereoGains {
    let (l, r) = mix.gains();
    match monitor {
        Monitor::Stereo => (l, r),
        Monitor::Left => (l, l),
        Monitor::Right => (r, r),
    }
}

/// Per output the first track name found in the songs.
fn playlist_track_names(songs: &[PlaylistSong], audio_lib: &str) -> [String; STRIPS] {
    let mut names: [String; STRIPS] = Default::default();
    for s in songs {
        let Ok(p) = SongProject::load(&s.songprj, audio_lib) else { continue };
        for (n, t) in names.iter_mut().zip(track_names(&p)) {
            if n.is_empty() {
                *n = t;
            }
        }
    }
    names
}

/// The strip meters of the two mixers. The engine measures one level per
/// strip, after the larger of its two preview gains; each side shows it
/// scaled to its own gain (off or muted on a side: nothing there). A side
/// that is not heard (monitor) shows nothing.
fn side_meters(shown: &[f32; STRIPS], mix: &BackupMix, monitor: Monitor) -> ([f32; STRIPS], [f32; STRIPS]) {
    let (pl, pr) = preview_gains(mix, monitor);
    let (l, r) = mix.gains();
    let scaled = |k: usize, gain: f32, heard: bool| {
        let measured = pl[k].max(pr[k]);
        if !heard || gain <= 0.0 || measured <= 0.0 {
            FLOOR_DB
        } else {
            (shown[k] + 20.0 * (gain / measured).log10()).max(FLOOR_DB)
        }
    };
    (
        std::array::from_fn(|k| scaled(k, l[k], monitor != Monitor::Right)),
        std::array::from_fn(|k| scaled(k, r[k], monitor != Monitor::Left)),
    )
}

/// "L: 1 2 3   R: 16   both: -   none: 4 5" for the outputs with a track
/// (or all, without names).
fn routing(mix: &BackupMix, names: &[String; STRIPS]) -> String {
    let any_names = names.iter().any(|n| !n.is_empty());
    let (mut l, mut r, mut both, mut none) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for k in 0..STRIPS {
        let n = (k + 1).to_string();
        match (mix.left.uses(k), mix.right.uses(k)) {
            (true, true) => both.push(n),
            (true, false) => l.push(n),
            (false, true) => r.push(n),
            (false, false) if !any_names || !names[k].is_empty() => none.push(n),
            _ => {}
        }
    }
    let list = |v: &Vec<String>| if v.is_empty() { "\u{2014}".to_string() } else { v.join(" ") };
    format!("Left: {}     Right: {}     Both: {}     Not used: {}", list(&l), list(&r), list(&both), list(&none))
}

impl BackupPanel {
    /// The playlists of the project library (called when it is read).
    pub fn set_library(&mut self, audio_lib: &str, playlists: Vec<(String, Vec<PlaylistSong>)>) {
        self.audio_lib = audio_lib.to_string();
        self.library = playlists;
        if let Some(p) = &self.playlist {
            if !self.library.iter().any(|(n, _)| n == p) {
                self.playlist = None;
            }
        }
        self.names_for = None; // read the names again
    }

    fn running(&self) -> bool {
        self.job.as_ref().map(|j| !j.lock().unwrap().finished).unwrap_or(false)
    }

    /// The gains the preview player should use now.
    pub fn preview(&self, mix: &BackupMix) -> StereoGains {
        preview_gains(mix, self.monitor)
    }

    fn songs(&self) -> Option<&Vec<PlaylistSong>> {
        let name = self.playlist.as_ref()?;
        self.library.iter().find(|(n, _)| n == name).map(|(_, s)| s)
    }

    /// Draw the tab; true if the settings changed (the caller saves them).
    pub fn ui(&mut self, ui: &mut Ui, settings: &mut BackupSettings, player: &mut RehearsalPanel) -> bool {
        let mut changed = false;
        let status = player.begin_frame(ui.ctx());
        let levels = player.take_levels();
        let shown = update_shown(ui, &mut self.shown, &levels);
        let running = self.running();
        if running {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        if self.playlist.is_none() {
            self.playlist = self.library.first().map(|(n, _)| n.clone());
        }
        if self.names_for != self.playlist {
            self.names = self.songs().map(|s| playlist_track_names(s, &self.audio_lib)).unwrap_or_default();
            self.first_song = self
                .songs()
                .and_then(|s| s.first())
                .map(|s| SongProject::load(&s.songprj, &self.audio_lib).map(|p| p.name).unwrap_or_else(|_| s.name.clone()))
                .unwrap_or_default();
            self.names_for = self.playlist.clone();
        }
        let song_count = self.songs().map(|s| s.len()).unwrap_or(0);

        // ---- settings row ----
        ui.add_enabled_ui(!running, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Playlist").size(14.0).color(TEXT));
                let names: Vec<String> = self.library.iter().map(|(n, _)| n.clone()).collect();
                if let Some(p) = playlist_combo(ui, "backup_playlist", 220.0, &names, self.playlist.as_deref(), false) {
                    self.playlist = Some(p);
                    self.message = None;
                }
                ui.label(RichText::new(format!("{} songs", song_count)).size(12.5).color(DIM));
                ui.add_space(16.0);
                ui.label(RichText::new("Target folder").size(14.0).color(TEXT));
                if folder_btn(ui, "Choose the folder for the MP3s") {
                    let mut dlg = rfd::FileDialog::new().set_title("Target folder for the MP3s");
                    if Path::new(&settings.dir).is_dir() {
                        dlg = dlg.set_directory(&settings.dir);
                    }
                    if let Some(dir) = dlg.pick_folder() {
                        settings.dir = dir.to_string_lossy().into_owned();
                        changed = true;
                    }
                }
                if ui.add(egui::TextEdit::singleline(&mut settings.dir).desired_width(320.0)).changed() {
                    changed = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("File names").size(14.0).color(TEXT));
                if ui
                    .add(egui::TextEdit::singleline(&mut settings.pattern).desired_width(220.0))
                    .on_hover_text("{nr} = position in the playlist (01, 02, ...), {song} = song name, {playlist} = playlist name")
                    .changed()
                {
                    changed = true;
                }
                let example = (song_count > 0)
                    .then(|| file_name(&settings.pattern, 1, song_count, &self.first_song, self.playlist.as_deref().unwrap_or("")));
                ui.label(
                    RichText::new(format!(
                        "{{nr}} {{song}} {{playlist}}   \u{2192}   {}",
                        example.unwrap_or_else(|| "\u{2014}".to_string())
                    ))
                    .size(12.5)
                    .color(DIM),
                );
                ui.add_space(16.0);
                ui.label(RichText::new("MP3 192 kbit/s CBR, stereo, best quality").size(12.5).color(DIM));
            });
        });
        ui.add_space(4.0);
        self.player_bar(ui, player, status.as_ref(), levels.clip, song_count);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if running {
                if small_btn(ui, "Cancel", true) {
                    if let Some(j) = &self.job {
                        j.lock().unwrap().cancel = true;
                    }
                }
            } else if ui
                .add_enabled(song_count > 0, egui::Button::new(RichText::new("Export MP3s").size(14.0).color(TEXT)).fill(ORANGE))
                .clicked()
            {
                self.start(ui.ctx(), settings);
            }
            ui.add_space(12.0);
            ui.label(RichText::new(routing(&settings.mix, &std::array::from_fn(|k| settings.name(k, &self.names[k]).to_string()))).size(12.5).color(TEXT));
        });
        if let Some(m) = &self.message {
            ui.label(RichText::new(m).size(12.5).color(RED));
        }
        self.job_view(ui);
        ui.add_space(4.0);

        // ---- the two mixers ----
        let h = ((ui.available_height() - 2.0 * 22.0) / 2.0).max(120.0);
        egui::ScrollArea::vertical().id_source("backup_mixers").auto_shrink([false, false]).show(ui, |ui| {
            ui.label(RichText::new("Left side").size(14.0).strong().color(TEXT));
            // the master meters show the side that is heard on them
            let (ml, mr) = match self.monitor {
                Monitor::Stereo => (shown[STRIPS], shown[STRIPS + 1]),
                Monitor::Left => (shown[STRIPS], FLOOR_DB),
                Monitor::Right => (FLOOR_DB, shown[STRIPS + 1]),
            };
            let strips: [f32; STRIPS] = std::array::from_fn(|k| shown[k]);
            let (left_strips, right_strips) = side_meters(&strips, &settings.mix, self.monitor);
            changed |=
                backup_side(ui, "bl", "Left", h, &mut settings.mix.left, &mut settings.names, &self.names, LEFT_FILL, !running, (left_strips, ml));
            ui.label(RichText::new("Right side").size(14.0).strong().color(TEXT));
            changed |=
                backup_side(ui, "br", "Right", h, &mut settings.mix.right, &mut settings.names, &self.names, RIGHT_FILL, !running, (right_strips, mr));
        });
        changed
    }

    /// The preview player: song, transport, time, what is heard, clip LED,
    /// and below it the timeline (click or drag to jump).
    fn player_bar(&mut self, ui: &mut Ui, player: &mut RehearsalPanel, status: Option<&Status>, clip: bool, song_count: usize) {
        let Some(playlist) = self.playlist.clone() else { return };
        let songs: Vec<String> = self.songs().map(|s| s.iter().map(|s| s.name.clone()).collect()).unwrap_or_default();
        // the player has this playlist loaded: its transport applies
        let ours = player.session.loaded_playlist() == Some(playlist.as_str());
        let st = status.filter(|s| ours && s.state != PlayState::Empty);
        if let Some(st) = st {
            player.keys(ui, st);
        }
        let playing = st.map(|s| s.state == PlayState::Playing).unwrap_or(false);
        let current = st.and_then(|s| s.index).unwrap_or(self.preview_index.min(song_count.saturating_sub(1)));
        egui::Frame::none().fill(PANEL).inner_margin(egui::Margin::same(6.0)).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("Preview").size(14.0).strong().color(ORANGE));
                let shown = songs.get(current).map(|n| format!("{:02}  {}", current + 1, n)).unwrap_or_else(|| "\u{2014}".to_string());
                let mut chosen = None;
                egui::ComboBox::from_id_source("backup_preview_song").width(260.0).selected_text(shown).show_ui(ui, |ui| {
                    for (i, n) in songs.iter().enumerate() {
                        if ui.selectable_label(i == current, format!("{:02}  {}", i + 1, n)).clicked() {
                            chosen = Some(i);
                        }
                    }
                });
                if let Some(i) = chosen {
                    self.preview_index = i;
                    player.session.preview_song(&playlist, i, playing);
                }
                ui.add_space(8.0);
                if ours {
                    player.transport(ui, status, TRANSPORT_SMALL);
                } else if ui
                    .add_enabled(song_count > 0, egui::Button::new(RichText::new("\u{25b6}  Play").size(14.0).color(TEXT)).min_size(vec2(80.0, 28.0)))
                    .on_hover_text("Load the playlist into the player and play the chosen song")
                    .clicked()
                {
                    player.session.preview_song(&playlist, current, true);
                }
                if let Some(st) = st {
                    let (text, dragging) = player.time_text(st);
                    ui.label(RichText::new(text).size(15.0).monospace().color(if dragging { ORANGE } else { TEXT }));
                }
                ui.add_space(12.0);
                ui.label(RichText::new("Listen to").size(13.0).color(DIM));
                for (m, label, tip) in [
                    (Monitor::Stereo, "L + R", "Both sides, as in the MP3"),
                    (Monitor::Left, "Left only", "Only the left side, on both speakers"),
                    (Monitor::Right, "Right only", "Only the right side, on both speakers"),
                ] {
                    if ui.selectable_label(self.monitor == m, RichText::new(label).size(13.0)).on_hover_text(tip).clicked() {
                        self.monitor = m;
                    }
                }
                ui.add_space(12.0);
                let (rect, resp) = ui.allocate_exact_size(vec2(56.0, 18.0), Sense::click());
                let p = ui.painter();
                p.circle_filled(egui::pos2(rect.left() + 9.0, rect.center().y), 5.0, if clip { RED } else { Color32::from_rgb(0x50, 0x28, 0x28) });
                p.text(egui::pos2(rect.left() + 20.0, rect.center().y), Align2::LEFT_CENTER, "CLIP", FontId::proportional(11.0), if clip { RED } else { DIM });
                if resp.on_hover_text("A side went above 0 dBFS (the MP3 would be clipped). Click to reset.").clicked() {
                    player.session.reset_clip();
                }
            });
            if let Some(st) = st {
                ui.add_space(4.0);
                let w = ui.available_width();
                player.progress(ui, st, w);
            }
            if let Some(e) = player.session.error() {
                ui.label(RichText::new(e).size(12.5).color(RED));
            } else if !ours && player.session.loaded_playlist().is_some() && status.map(|s| s.state == PlayState::Playing).unwrap_or(false) {
                ui.label(
                    RichText::new("The player is playing another playlist (Rehearsal tab); Play loads this one.")
                        .size(12.5)
                        .color(DIM),
                );
            }
            if let Some(st) = st {
                for w in st.warnings.iter() {
                    ui.label(RichText::new(format!("Warning: {}", w)).size(12.5).color(ORANGE));
                }
            }
        });
    }

    /// Progress bar while running, the report when done.
    fn job_view(&mut self, ui: &mut Ui) {
        let Some(job) = self.job.clone() else { return };
        let j = job.lock().unwrap();
        ui.add_space(4.0);
        if !j.finished {
            let frac = if j.total > 0 { ((j.done as f64 + j.frac) / j.total as f64) as f32 } else { 0.0 };
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
            ui.painter().rect_stroke(rect, 0.0, Stroke::new(1.0_f32, ORANGE));
            let mut fill = rect.shrink(2.0);
            fill.set_width(fill.width() * frac.clamp(0.0, 1.0));
            ui.painter().rect_filled(fill, 2.0, ORANGE);
            ui.painter().text(
                rect.left_center() + vec2(8.0, 0.0),
                Align2::LEFT_CENTER,
                format!("Song {} of {}: {}", (j.done + 1).min(j.total), j.total, j.current),
                FontId::proportional(12.5),
                Color32::WHITE,
            );
            return;
        }
        let errors = j.lines.iter().filter(|l| !l.ok).count();
        let notes = j.lines.iter().filter(|l| !l.notes.is_empty()).count();
        let title = if j.cancel {
            "Export cancelled.".to_string()
        } else if errors > 0 {
            format!("Export finished with {} error(s).", errors)
        } else {
            format!("{} MP3(s) written to {} in {}.", j.lines.len(), j.dir, fmt_clock(j.elapsed))
        };
        let color = if errors > 0 || j.cancel { RED } else if notes > 0 { ORANGE } else { GREEN };
        let mut close = false;
        egui::Frame::none().fill(PANEL).inner_margin(egui::Margin::same(6.0)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).size(14.0).color(color));
                if small_btn(ui, "Close report", true) {
                    close = true;
                }
            });
            egui::ScrollArea::vertical().id_source("backup_report").max_height(REPORT_H).auto_shrink([false, true]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                for l in &j.lines {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("- {}: ", l.name)).size(13.0));
                        ui.label(RichText::new(&l.text).size(13.0).color(if l.ok { GREEN } else { RED }));
                    });
                    for n in &l.notes {
                        ui.label(RichText::new(format!("      {}", n)).size(12.5).color(ORANGE));
                    }
                }
            });
        });
        drop(j);
        if close {
            self.job = None;
        }
    }

    /// Check the settings and start the export thread.
    fn start(&mut self, ctx: &egui::Context, settings: &BackupSettings) {
        self.message = None;
        let Some(songs) = self.songs().cloned() else {
            self.message = Some("Choose a playlist.".to_string());
            return;
        };
        let dir = settings.dir.trim().to_string();
        if dir.is_empty() {
            self.message = Some("Choose a target folder.".to_string());
            return;
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.message = Some(format!("The target folder cannot be created: {}", e));
            return;
        }
        let mix = settings.mix;
        let (l, r) = mix.gains();
        if l.iter().chain(r.iter()).all(|g| *g == 0.0) {
            self.message = Some("No output is turned up on the left or the right side.".to_string());
            return;
        }
        let job = Arc::new(Mutex::new(Job {
            total: songs.len(),
            done: 0,
            current: String::new(),
            frac: 0.0,
            lines: Vec::new(),
            finished: false,
            cancel: false,
            started: Instant::now(),
            elapsed: 0.0,
            dir: dir.clone(),
        }));
        self.job = Some(job.clone());
        let audio_lib = self.audio_lib.clone();
        let playlist = self.playlist.clone().unwrap_or_default();
        let pattern = settings.pattern.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            run(&job, &songs, &audio_lib, &playlist, &pattern, &mix, Path::new(&dir));
            let mut j = job.lock().unwrap();
            j.elapsed = j.started.elapsed().as_secs_f64();
            j.finished = true;
            drop(j);
            ctx.request_repaint();
        });
    }
}

/// The export thread: all songs one after the other.
fn run(job: &Mutex<Job>, songs: &[PlaylistSong], audio_lib: &str, playlist: &str, pattern: &str, mix: &BackupMix, dir: &Path) {
    let total = songs.len();
    let projects: Vec<Result<SongProject, String>> =
        songs.iter().map(|s| SongProject::load(&s.songprj, audio_lib).map_err(|e| e.to_string())).collect();
    let names = unique_names(
        projects
            .iter()
            .zip(songs)
            .enumerate()
            .map(|(i, (p, s))| {
                let song = p.as_ref().map(|p| p.name.clone()).unwrap_or_else(|_| s.name.clone());
                file_name(pattern, i + 1, total, &song, playlist)
            })
            .collect(),
    );
    for (i, (project, file)) in projects.into_iter().zip(names).enumerate() {
        {
            let mut j = job.lock().unwrap();
            if j.cancel {
                return;
            }
            j.current = file.clone();
            j.frac = 0.0;
        }
        let line = match project {
            Err(e) => Line { name: file, ok: false, text: e, notes: Vec::new() },
            Ok(p) => {
                let tags = Tags { title: p.name.clone(), album: playlist.to_string(), track: i + 1, total };
                let dest: PathBuf = dir.join(&file);
                let mut progress = |f: f64| {
                    let mut j = job.lock().unwrap();
                    j.frac = f;
                    !j.cancel
                };
                match render_song(&p, mix, &tags, &dest, &mut progress) {
                    Ok(rep) => {
                        let mut notes = rep.warnings;
                        if rep.clipped > 0 {
                            notes.insert(
                                0,
                                format!(
                                    "Clipped: peak {:+.1} dBFS, {} samples limited \u{2014} lower the master of that side and export again",
                                    rep.peak_db, rep.clipped
                                ),
                            );
                        }
                        Line { name: file, ok: true, text: fmt_clock(rep.seconds), notes }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return,
                    Err(e) => Line { name: file, ok: false, text: format!("ERROR: {}", e), notes: Vec::new() },
                }
            }
        };
        let mut j = job.lock().unwrap();
        j.lines.push(line);
        j.done += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preview_hears_the_mp3_or_one_side() {
        let mut m = BackupMix::default();
        m.left.set_level(0, 0.0);
        m.right.set_level(15, 0.0);
        let (l, r) = preview_gains(&m, Monitor::Stereo);
        assert_eq!((l[0], r[0], l[15], r[15]), (1.0, 0.0, 0.0, 1.0));
        let (l, r) = preview_gains(&m, Monitor::Left);
        assert_eq!((l[0], r[0], l[15], r[15]), (1.0, 1.0, 0.0, 0.0));
        let (l, r) = preview_gains(&m, Monitor::Right);
        assert_eq!((l[0], r[0], l[15], r[15]), (0.0, 0.0, 1.0, 1.0));
    }

    #[test]
    fn a_strip_meter_shows_only_on_the_side_it_goes_to() {
        let mut m = BackupMix::default();
        m.right.set_level(15, 0.0); // click: right only
        m.left.set_level(0, 0.0); // backing on both, right 6 dB lower
        m.right.set_level(0, -6.0);
        let mut shown = [FLOOR_DB; STRIPS];
        shown[15] = -3.0;
        shown[0] = -10.0;
        let (l, r) = side_meters(&shown, &m, Monitor::Stereo);
        assert_eq!(l[15], FLOOR_DB, "the click is not on the left");
        assert_eq!(r[15], -3.0);
        assert!((l[0] + 10.0).abs() < 1e-3 && (r[0] + 16.0).abs() < 0.01, "{} {}", l[0], r[0]);
        // one side heard: the other shows nothing
        let (l, r) = side_meters(&shown, &m, Monitor::Right);
        assert_eq!(l, [FLOOR_DB; STRIPS]);
        assert_eq!(r[15], -3.0);
        let (l, r) = side_meters(&shown, &m, Monitor::Left);
        assert!((l[0] + 10.0).abs() < 1e-3);
        assert_eq!(r, [FLOOR_DB; STRIPS]);
        // muted on a side: nothing there
        m.right.set_mute(15, true);
        assert_eq!(side_meters(&shown, &m, Monitor::Stereo).1[15], FLOOR_DB);
    }

    #[test]
    fn routing_line() {
        let mut m = BackupMix::default();
        m.left.set_level(0, 0.0);
        m.left.set_level(1, 0.0);
        m.right.set_level(15, 0.0);
        m.right.set_level(1, -6.0);
        let mut names: [String; STRIPS] = Default::default();
        names[0] = "Synth".into();
        names[1] = "Pad".into();
        names[2] = "Bass".into();
        names[15] = "Click".into();
        assert_eq!(routing(&m, &names), "Left: 1     Right: 16     Both: 2     Not used: 3");
        // without names every output that is not used counts
        let none = routing(&BackupMix::default(), &Default::default());
        assert!(none.starts_with("Left: \u{2014}     Right: \u{2014}     Both: \u{2014}     Not used: 1 2 3"), "{}", none);
    }
}

//! Band package export and import (`lp16_tool::bandpack`): the file
//! dialogs, the import preview and the progress of the background thread.
//! The export starts from a playlist in Home, the import from the Import tab.

use crate::gui_app::{disk_free, fmt_date, fmt_size, overlay, DIM, ORANGE, RED, TEXT};
use crate::gui_mixer::GREEN;
use eframe::egui::{self, vec2, Align2, Color32, FontId, RichText, Sense, Stroke};
use lp16_tool::bandpack::{self, ImportPlan, Package, SongAction};
use lp16_tool::project_lib::ProjectLibrary;
use lp16_tool::tr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// The running (or finished) export or import.
struct Job {
    import: bool,
    /// the playlist of an import (selected in Home afterwards)
    playlist: String,
    total: u64,
    done: u64,
    current: String,
    cancel: bool,
    /// None while running; Ok(text) / Err(text) when finished
    result: Option<Result<String, String>>,
    started: Instant,
}

/// An opened package, waiting for "Import".
struct Preview {
    pkg: Arc<Package>,
    plan: Arc<ImportPlan>,
    free: Option<u64>,
}

#[derive(Default)]
pub struct BandPack {
    job: Option<Arc<Mutex<Job>>>,
    preview: Option<Preview>,
}

impl BandPack {
    /// Pack playlist `name`: check it, ask for the file, start the thread.
    /// Err: a message for the user.
    pub fn export(&mut self, ctx: &egui::Context, lib: &ProjectLibrary, name: &str) -> Result<(), String> {
        let plan = bandpack::plan_export(lib, name)?;
        let Some(dest) = rfd::FileDialog::new()
            .set_title(tr!("pack.export_title"))
            .add_filter(tr!("pack.filter"), &[bandpack::EXT])
            .set_file_name(format!("{}.{}", name, bandpack::EXT))
            .save_file()
        else {
            return Ok(());
        };
        let dest = if dest.extension().is_none() { dest.with_extension(bandpack::EXT) } else { dest };
        if let (Some(free), Some(dir)) = (dest.parent().and_then(|d| disk_free(&d.to_string_lossy())), dest.parent()) {
            if free < plan.bytes() {
                return Err(tr!("pack.no_space", need = fmt_size(plan.bytes()), free = fmt_size(free), dir = dir.display()));
            }
        }
        let job = self.new_job(false, name, plan.bytes());
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = bandpack::write_package(&plan, &dest, &mut progress(&job, &ctx));
            let text = match result {
                Ok(()) => Ok(tr!(
                    "pack.export_done",
                    file = dest.display(),
                    songs = plan.song_count(),
                    files = plan.file_count(),
                    size = fmt_size(plan.bytes())
                )),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Err(tr!("pack.canceled").to_string()),
                Err(e) => Err(e.to_string()),
            };
            finish(&job, &ctx, text);
        });
        Ok(())
    }

    /// Ask for a package and show what an import would do.
    pub fn open(&mut self, lib: &ProjectLibrary) -> Result<(), String> {
        let Some(path) = rfd::FileDialog::new()
            .set_title(tr!("pack.import_title"))
            .add_filter(tr!("pack.filter"), &[bandpack::EXT])
            .pick_file()
        else {
            return Ok(());
        };
        let pkg = bandpack::open_package(&path)?;
        let plan = bandpack::plan_import(&pkg, lib)?;
        let free = disk_free(lib.audio_lib()).or_else(|| disk_free(&drive_of(lib.audio_lib())));
        self.preview = Some(Preview { pkg: Arc::new(pkg), plan: Arc::new(plan), free });
        Ok(())
    }

    fn new_job(&mut self, import: bool, playlist: &str, total: u64) -> Arc<Mutex<Job>> {
        let job = Arc::new(Mutex::new(Job {
            import,
            playlist: playlist.to_string(),
            total,
            done: 0,
            current: String::new(),
            cancel: false,
            result: None,
            started: Instant::now(),
        }));
        self.job = Some(job.clone());
        job
    }

    fn start_import(&mut self, ctx: &egui::Context, lib: ProjectLibrary) {
        let Some(p) = self.preview.take() else { return };
        let job = self.new_job(true, &p.plan.playlist, p.plan.bytes());
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = bandpack::run_import(&p.pkg, &p.plan, &lib, &mut progress(&job, &ctx));
            let text = match result {
                Ok(()) => {
                    let count = |a: SongAction| p.plan.songs.iter().filter(|s| s.action == a).count();
                    Ok(tr!(
                        "pack.import_done",
                        playlist = p.plan.playlist,
                        new = count(SongAction::New),
                        updated = count(SongAction::Update),
                        same = count(SongAction::Same),
                        size = fmt_size(p.plan.bytes())
                    ))
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Err(tr!("pack.import_canceled").to_string()),
                Err(e) => Err(e.to_string()),
            };
            finish(&job, &ctx, text);
        });
    }

    /// Draw the preview or the job. Some after an import (the library must be
    /// read again): the imported playlist, or "" if the import failed.
    pub fn ui(&mut self, ctx: &egui::Context, lib: &ProjectLibrary) -> Option<String> {
        if self.preview.is_some() {
            self.preview_ui(ctx, lib);
            return None;
        }
        let job = self.job.clone()?;
        let j = job.lock().unwrap();
        let title = if j.import { tr!("pack.import_title") } else { tr!("pack.export_title") };
        match &j.result {
            None => {
                let (done, total, current) = (j.done, j.total, j.current.clone());
                let secs = j.started.elapsed().as_secs_f64();
                drop(j);
                let clicked = overlay(ctx, "bandpack_progress", egui::Order::Foreground, title, vec2(900.0, 160.0), |ui| {
                    let frac = if total > 0 { (done as f64 / total as f64) as f32 } else { 0.0 };
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
                    ui.painter().rect_stroke(rect, 0.0, Stroke::new(1.0_f32, ORANGE));
                    let mut fill = rect.shrink(2.0);
                    fill.set_width(fill.width() * frac);
                    ui.painter().rect_filled(fill, 2.0, ORANGE);
                    ui.painter().text(rect.left_center() + vec2(8.0, 0.0), Align2::LEFT_CENTER, &current, FontId::proportional(12.0), Color32::WHITE);
                    ui.add_space(12.0);
                    // remaining time from the speed so far
                    let rest = if done > 0 && secs > 2.0 { Some((total - done.min(total)) as f64 * secs / done as f64) } else { None };
                    let line = match rest {
                        Some(r) => tr!("pack.progress_eta", done = fmt_size(done), total = fmt_size(total), min = (r / 60.0).ceil() as u64),
                        None => tr!("pack.progress", done = fmt_size(done), total = fmt_size(total)),
                    };
                    ui.vertical_centered(|ui| ui.label(RichText::new(line).size(15.0).color(TEXT)));
                }, &[(tr!("common.cancel"), true)]);
                if clicked.is_some() {
                    job.lock().unwrap().cancel = true;
                }
                None
            }
            Some(result) => {
                let (ok, text) = match result {
                    Ok(t) => (true, t.clone()),
                    Err(t) => (false, t.clone()),
                };
                let (import, playlist) = (j.import, j.playlist.clone());
                drop(j);
                let clicked = overlay(ctx, "bandpack_done", egui::Order::Foreground, title, vec2(900.0, 220.0), |ui| {
                    ui.label(RichText::new(&text).size(14.0).color(if ok { TEXT } else { RED }));
                }, &[(tr!("common.ok"), true)]);
                if clicked.is_some() {
                    self.job = None;
                    // also a canceled import may have copied files
                    if import {
                        return Some(if ok { playlist } else { String::new() });
                    }
                }
                None
            }
        }
    }

    fn preview_ui(&mut self, ctx: &egui::Context, lib: &ProjectLibrary) {
        let p = self.preview.as_ref().unwrap();
        let plan = &p.plan;
        let need = plan.bytes();
        let enough = p.free.map(|f| f >= need).unwrap_or(true);
        let clicked = overlay(ctx, "bandpack_preview", egui::Order::Foreground, tr!("pack.import_title"), vec2(900.0, 560.0), |ui| {
            let pl_state = if plan.playlist_exists { tr!("pack.pl_replaced") } else { tr!("pack.pl_new") };
            ui.label(RichText::new(tr!("pack.preview_playlist", name = plan.playlist, state = pl_state)).size(16.0).color(TEXT));
            ui.label(RichText::new(tr!("pack.preview_from", app = p.pkg.app, date = fmt_date(p.pkg.created))).size(11.5).color(DIM));
            ui.add_space(8.0);
            let list_h = (ui.available_height() - 90.0).max(80.0);
            egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("pack_songs").num_columns(3).striped(true).spacing([18.0, 4.0]).show(ui, |ui| {
                    for h in [tr!("import.col_song"), tr!("import.col_status"), tr!("pack.col_copy")] {
                        ui.label(RichText::new(h).color(DIM));
                    }
                    ui.end_row();
                    for s in &plan.songs {
                        ui.label(RichText::new(&s.name).color(TEXT));
                        let (text, color) = match s.action {
                            SongAction::New => (tr!("pack.st_new"), ORANGE),
                            SongAction::Update => (tr!("pack.st_update"), ORANGE),
                            SongAction::Same => (tr!("pack.st_same"), GREEN),
                        };
                        ui.label(RichText::new(text).color(color));
                        ui.label(RichText::new(if s.bytes > 0 { fmt_size(s.bytes) } else { "-".into() }).color(TEXT));
                        ui.end_row();
                    }
                });
            });
            ui.add_space(8.0);
            let size = tr!("pack.preview_copy", n = plan.file_count(), size = fmt_size(need));
            let free = p.free.map(|f| tr!("pack.preview_free", free = fmt_size(f))).unwrap_or_default();
            ui.label(RichText::new(format!("{}{}", size, free)).color(if enough { TEXT } else { RED }));
            ui.label(RichText::new(tr!("pack.preview_note")).size(11.5).color(DIM));
        }, &[(tr!("pack.import_btn"), enough), (tr!("common.cancel"), true)]);
        match clicked {
            Some(0) => self.start_import(ctx, lib.clone()),
            Some(_) => self.preview = None,
            None => {}
        }
    }
}

/// The progress callback of the thread: stores the bytes, says whether to go on.
fn progress<'a>(job: &'a Arc<Mutex<Job>>, ctx: &'a egui::Context) -> impl FnMut(u64, &str) -> bool + 'a {
    let mut last = Instant::now();
    move |done, current| {
        let mut j = job.lock().unwrap();
        j.done = done;
        if j.current != current {
            j.current = current.to_string();
        }
        // repaint a few times a second, not for every block
        if last.elapsed().as_millis() > 100 {
            last = Instant::now();
            ctx.request_repaint();
        }
        !j.cancel
    }
}

fn finish(job: &Mutex<Job>, ctx: &egui::Context, result: Result<String, String>) {
    job.lock().unwrap().result = Some(result);
    ctx.request_repaint();
}

/// "E:\" of "E:\LP16\AudioLibrary".
fn drive_of(path: &str) -> String {
    PathBuf::from(path).components().next().map(|c| format!("{}\\", c.as_os_str().to_string_lossy())).unwrap_or_default()
}


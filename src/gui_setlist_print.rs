//! "Print setlist" of a playlist in Home: a dialog with the heading, the
//! band logo and the print title of every song of the concert, then a Word
//! file (`lp16_tool::setlist_doc`). Print titles changed here are stored at
//! the songs (`.songinfo`), like in the song editor.

use crate::gui_app::{overlay, small_btn, DIM, MARKER_FILL, ORANGE, RED, TEXT};
use eframe::egui::{self, vec2, Margin, RichText};
use lp16_tool::project_lib::ProjectLibrary;
use lp16_tool::setlist::SetMarkers;
use lp16_tool::setlist_doc::{self, Logo, SetlistDoc};
use lp16_tool::song_info::{self, SongInfo};
use lp16_tool::tr;
use std::path::{Path, PathBuf};

struct Row {
    /// the .songprj
    prj: PathBuf,
    /// file stem of the song
    name: String,
    /// the stored print title, or the one made from the song name
    title: String,
    /// `title` when the dialog was opened (only a change is stored)
    initial: String,
    print: bool,
}

struct Form {
    playlist: String,
    heading: String,
    rows: Vec<Row>,
    /// index into `rows` of the first encore
    encore_at: Option<usize>,
    error: Option<String>,
}

/// What the dialog changed this frame.
#[derive(Default)]
pub struct PrintOutcome {
    /// print titles were stored (the library is to be read again)
    pub titles_changed: bool,
    /// a new logo was chosen ("" = none)
    pub logo: Option<String>,
}

#[derive(Default)]
pub struct SetlistPrint {
    form: Option<Form>,
    /// the written file, or why it failed
    done: Option<Result<PathBuf, String>>,
}

impl SetlistPrint {
    /// Open the dialog for `playlist` with its songs (.songprj paths); only
    /// the songs before the end of the concert are printed.
    pub fn open(&mut self, playlist: &str, songs: &[String], markers: SetMarkers) {
        let m = markers.clamp(songs.len());
        let end = m.end.unwrap_or(songs.len());
        let rows = songs[..end]
            .iter()
            .map(|p| {
                let prj = PathBuf::from(p);
                let name = prj.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let stored = song_info::load(&prj).print_title;
                let title = if stored.is_empty() { song_info::default_print_title(&name) } else { stored };
                Row { prj, name, initial: title.clone(), title, print: true }
            })
            .collect();
        self.form = Some(Form {
            playlist: playlist.to_string(),
            heading: setlist_doc::default_heading(playlist),
            rows,
            encore_at: m.encore.filter(|&e| e < end),
            error: None,
        });
    }

    /// Draw the dialog or the result. `logo` is the stored logo path.
    pub fn ui(&mut self, ctx: &egui::Context, lib: &ProjectLibrary, logo: &str) -> PrintOutcome {
        let mut out = PrintOutcome::default();
        if let Some(done) = &self.done {
            let (text, color, ok) = match done {
                Ok(p) => (tr!("print.done", file = p.display()), TEXT, true),
                Err(e) => (e.clone(), RED, false),
            };
            let buttons: Vec<(&str, bool)> = if ok {
                vec![(tr!("print.open"), true), (tr!("common.ok"), true)]
            } else {
                vec![(tr!("common.ok"), true)]
            };
            let clicked = overlay(ctx, "setlist_done", egui::Order::Foreground, tr!("print.title"), vec2(760.0, 200.0), |ui| {
                ui.label(RichText::new(text).size(14.0).color(color));
            }, &buttons);
            if let Some(i) = clicked {
                let opened = match &self.done {
                    Some(Ok(p)) if ok && i == 0 => open_file(p),
                    _ => Ok(()),
                };
                // a file that does not open: say why
                self.done = opened.err().map(Err);
            }
            return out;
        }
        let Some(f) = self.form.as_mut() else { return out };
        let mut pick_logo = false;
        let mut no_logo = false;
        let printed = f.rows.iter().filter(|r| r.print).count();
        let clicked = overlay(ctx, "setlist_print", egui::Order::Foreground, tr!("print.title"), vec2(900.0, 760.0), |ui| {
            egui::Grid::new("print_head").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label(RichText::new(tr!("print.heading")).color(TEXT));
                ui.add(egui::TextEdit::singleline(&mut f.heading).desired_width(ui.available_width() - 10.0));
                ui.end_row();
                ui.label(RichText::new(tr!("print.logo")).color(TEXT));
                ui.horizontal(|ui| {
                    if small_btn(ui, tr!("print.logo_pick"), true) {
                        pick_logo = true;
                    }
                    if small_btn(ui, tr!("print.logo_none"), !logo.is_empty()) {
                        no_logo = true;
                    }
                    let shown = if logo.is_empty() { tr!("print.no_logo").to_string() } else { logo.to_string() };
                    ui.label(RichText::new(shown).color(DIM));
                });
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.label(RichText::new(tr!("print.hint")).size(12.0).color(DIM));
            ui.add_space(6.0);
            let list_h = (ui.available_height() - 40.0).max(100.0);
            egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
                let mut n = 0;
                let title_w = (ui.available_width() * 0.55).max(200.0);
                for (i, r) in f.rows.iter_mut().enumerate() {
                    if f.encore_at == Some(i) {
                        egui::Frame::none().fill(MARKER_FILL).inner_margin(Margin::symmetric(8.0, 2.0)).show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.label(RichText::new(tr!("print.encore_line")).color(ORANGE));
                        });
                    }
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut r.print, "").on_hover_text(tr!("print.include"));
                        let nr = if r.print {
                            n += 1;
                            format!("{:>2}.", n)
                        } else {
                            "  ".to_string()
                        };
                        ui.label(RichText::new(nr).monospace().color(if r.print { TEXT } else { DIM }));
                        ui.add_enabled(
                            r.print,
                            egui::TextEdit::singleline(&mut r.title).desired_width(title_w),
                        );
                        ui.label(RichText::new(&r.name).size(11.5).color(DIM));
                    });
                }
            });
            if let Some(e) = &f.error {
                ui.label(RichText::new(e).color(RED));
            }
        }, &[(tr!("print.save"), printed > 0), (tr!("common.cancel"), true)]);
        if pick_logo {
            if let Some(p) = rfd::FileDialog::new()
                .set_title(tr!("print.logo"))
                .add_filter(tr!("print.image_filter"), &["png", "jpg", "jpeg"])
                .pick_file()
            {
                match Logo::read(&p) {
                    Ok(_) => {
                        f.error = None;
                        out.logo = Some(p.to_string_lossy().into_owned());
                    }
                    Err(e) => f.error = Some(e),
                }
            }
        }
        if no_logo {
            out.logo = Some(String::new());
        }
        match clicked {
            Some(0) => {
                let logo_now = out.logo.clone().unwrap_or_else(|| logo.to_string());
                match self.export(lib, &logo_now, &mut out) {
                    Ok(Some(path)) => {
                        self.form = None;
                        self.done = Some(Ok(path));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        if let Some(f) = self.form.as_mut() {
                            f.error = Some(e);
                        }
                    }
                }
            }
            Some(_) => self.form = None,
            None => {}
        }
        out
    }

    /// Store the changed print titles, ask for the file and write it.
    /// Ok(None): the file dialog was canceled.
    fn export(&mut self, lib: &ProjectLibrary, logo: &str, out: &mut PrintOutcome) -> Result<Option<PathBuf>, String> {
        let f = self.form.as_mut().unwrap();
        for r in f.rows.iter_mut() {
            let title = r.title.trim().to_string();
            if title != r.initial.trim() {
                lib.save_song_info(&r.prj, &SongInfo { print_title: title.clone() })?;
                r.initial = title.clone();
                r.title = title;
                out.titles_changed = true;
            }
        }
        let logo = if logo.is_empty() { None } else { Some(Logo::read(Path::new(logo))?) };
        let mut songs = Vec::new();
        let mut encore_at = None;
        for (i, r) in f.rows.iter().enumerate() {
            if !r.print {
                continue;
            }
            if encore_at.is_none() && f.encore_at.map(|e| i >= e).unwrap_or(false) {
                encore_at = Some(songs.len());
            }
            let t = r.title.trim();
            songs.push(if t.is_empty() { song_info::default_print_title(&r.name) } else { t.to_string() });
        }
        let doc = SetlistDoc { heading: f.heading.trim().to_string(), songs, encore_at, logo };
        let Some(dest) = rfd::FileDialog::new()
            .set_title(tr!("print.title"))
            .add_filter(tr!("print.docx_filter"), &["docx"])
            .set_file_name(format!("{}_Setlist.docx", f.playlist))
            .save_file()
        else {
            return Ok(None);
        };
        let dest = if dest.extension().is_none() { dest.with_extension("docx") } else { dest };
        let bytes = setlist_doc::docx(&doc).map_err(|e| e.to_string())?;
        lp16_tool::write_bytes_atomic(&dest, &bytes).map_err(|e| tr!("print.cannot_write", file = dest.display(), err = e))?;
        Ok(Some(dest))
    }
}

/// Open the file with its program (Word, LibreOffice), like a double click
/// in the Explorer.
#[cfg(windows)]
fn open_file(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut std::ffi::c_void,
            op: *const u16,
            file: *const u16,
            params: *const u16,
            dir: *const u16,
            show: i32,
        ) -> isize;
    }
    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let op = wide(std::ffi::OsStr::new("open"));
    let file = wide(path.as_os_str());
    // SW_SHOWNORMAL; a result above 32 means success
    let r = unsafe { ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), 1) };
    if r > 32 {
        Ok(())
    } else {
        Err(tr!("print.cannot_open", file = path.display(), err = r))
    }
}

#[cfg(not(windows))]
fn open_file(path: &Path) -> Result<(), String> {
    std::process::Command::new("xdg-open").arg(path).spawn().map(|_| ()).map_err(|e| tr!("print.cannot_open", file = path.display(), err = e))
}


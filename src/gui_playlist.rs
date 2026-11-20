//! The left part of the Rehearsal tab: the playlist combo and the song
//! list. The rows come from the library (`rehearsal_queue::song_rows`); this
//! only draws them and reports what was clicked.

use crate::gui_app::{DIM, LINE, ORANGE, PANEL, RED, TEXT};
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Sense, Stroke, Ui};
use lp16_tool::rehearsal_queue::SongRow;

/// Label of the combo entry for a single song.
pub const SINGLE_SONG: &str = "(single song)";

/// What the user did in the list during this frame.
#[derive(Default)]
pub struct ListOutcome {
    /// a playlist was chosen in the combo
    pub select: Option<String>,
    /// a song was double-clicked
    pub play: Option<usize>,
}

/// Combo and song list. `playlists` are the names of the library,
/// `selected` the loaded one (None: a single song, or nothing).
pub fn ui(ui: &mut Ui, playlists: &[String], selected: Option<&str>, single: bool, rows: &[SongRow]) -> ListOutcome {
    let mut out = ListOutcome::default();
    ui.label(egui::RichText::new("Playlist").size(13.0).color(DIM));
    let width = ui.available_width() - 8.0;
    out.select = playlist_combo(ui, "rehearsal_playlist", width, playlists, selected, single);
    ui.add_space(8.0);
    if rows.is_empty() {
        let text = if playlists.is_empty() { "The library has no playlists." } else { "Choose a playlist." };
        ui.label(egui::RichText::new(text).size(13.0).color(DIM));
        return out;
    }
    for row in rows {
        if let Some(r) = song_row(ui, row) {
            out.play = Some(r);
        }
    }
    out
}

/// The playlist combo; returns the playlist that was chosen. `selected` is
/// the loaded playlist (None: a single song, or nothing).
pub fn playlist_combo(
    ui: &mut Ui,
    id: &str,
    width: f32,
    playlists: &[String],
    selected: Option<&str>,
    single: bool,
) -> Option<String> {
    let mut chosen = None;
    let shown = match (selected, single) {
        (Some(n), _) => n.to_string(),
        (None, true) => SINGLE_SONG.to_string(),
        (None, false) => "(none)".to_string(),
    };
    egui::ComboBox::from_id_source(id).width(width).selected_text(shown).show_ui(ui, |ui| {
        if single && selected.is_none() {
            let _ = ui.selectable_label(true, SINGLE_SONG);
        }
        for name in playlists {
            if ui.selectable_label(selected == Some(name.as_str()), name).clicked() && selected != Some(name.as_str()) {
                chosen = Some(name.clone());
            }
        }
    });
    chosen
}

/// The text of a song combo entry: "3  Name".
pub fn song_label(row: &SongRow) -> String {
    format!("{}  {}", row.number, row.name)
}

/// A compact song list as a combo: choosing a song returns its index
/// (0-based), like a double-click in the list. Unreadable songs are shown
/// grey and cannot be chosen.
pub fn song_combo(ui: &mut Ui, id: &str, width: f32, rows: &[SongRow]) -> Option<usize> {
    let mut chosen = None;
    let current = rows.iter().find(|r| r.current).map(song_label).unwrap_or_else(|| "(no song)".to_string());
    egui::ComboBox::from_id_source(id).width(width).selected_text(current).show_ui(ui, |ui| {
        for row in rows {
            let text = song_label(row);
            let resp = match &row.error {
                Some(e) => ui
                    .add_enabled(false, egui::SelectableLabel::new(false, text))
                    .on_disabled_hover_text(format!("Cannot be played, skipped: {}", e)),
                None => ui.selectable_label(row.current, text),
            };
            if resp.clicked() {
                chosen = Some(row.number - 1);
            }
        }
    });
    chosen
}

/// One row; returns its index (0-based) when double-clicked.
fn song_row(ui: &mut Ui, row: &SongRow) -> Option<usize> {
    let w = ui.available_width() - 8.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 26.0), Sense::click());
    let p = ui.painter();
    if row.current {
        p.rect_filled(rect, 3.0, Color32::from_rgba_unmultiplied(0xf0, 0x6b, 0x19, 0x70));
    } else if resp.hovered() {
        p.rect_filled(rect, 3.0, PANEL);
    }
    if row.next {
        p.rect_stroke(rect.shrink(1.0), 3.0, Stroke::new(1.5_f32, ORANGE));
    }
    let color = if row.error.is_some() { RED } else { TEXT };
    let mid = rect.center().y;
    p.text(pos2(rect.left() + 8.0, mid), Align2::LEFT_CENTER, row.number.to_string(), FontId::proportional(13.0), DIM);
    // right side first: length, MIDI symbol
    let mut right = rect.right() - 8.0;
    if !row.length.is_empty() {
        p.text(pos2(right, mid), Align2::RIGHT_CENTER, &row.length, FontId::proportional(13.0), color);
        right -= 40.0;
    }
    if row.midi {
        p.text(pos2(right, mid), Align2::RIGHT_CENTER, "\u{266a}", FontId::proportional(14.0), if row.current { TEXT } else { ORANGE });
        right -= 18.0;
    }
    // the name, cut to the space that is left
    let name_rect = egui::Rect::from_min_max(pos2(rect.left() + 32.0, rect.top()), pos2(right, rect.bottom()));
    p.with_clip_rect(name_rect).text(
        pos2(name_rect.left(), mid),
        Align2::LEFT_CENTER,
        if row.next && !row.current { format!("\u{25b6} {}", row.name) } else { row.name.clone() },
        FontId::proportional(14.5),
        color,
    );
    p.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(0.5_f32, LINE));
    let resp = match &row.error {
        Some(e) => resp.on_hover_text(format!("Cannot be played, skipped: {}", e)),
        None => resp,
    };
    if resp.double_clicked() && row.error.is_none() {
        Some(row.number - 1)
    } else {
        None
    }
}

//! The left part of the Rehearsal tab: the playlist combo and the song
//! list. The rows come from the library (`rehearsal_queue::song_rows`); this
//! only draws them and reports what was clicked. Between the songs it shows
//! the concert markers of the playlist (encores, end of concert).

use crate::gui_app::{DIM, LINE, MARKER_FILL, ORANGE, PANEL, RED, TEXT};
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Sense, Stroke, Ui};
use lp16_tool::rehearsal_queue::SongRow;
use lp16_tool::setlist::SetMarkers;

/// Label of the combo entry for a single song.
pub const SINGLE_SONG: &str = "(single song)";

/// What the user did in the list during this frame.
#[derive(Default)]
pub struct ListOutcome {
    /// a playlist was chosen in the combo
    pub select: Option<String>,
    /// "(single song)" was chosen: all songs of the library
    pub library: bool,
    /// a song was double-clicked: start it
    pub play: Option<usize>,
    /// a song was clicked: mark it (or set it as the next one while playing)
    pub click: Option<usize>,
}

/// Combo and song list. `playlists` are the names of the library,
/// `selected` the loaded one (None: single songs, or nothing); `single`:
/// single songs are loaded; `has_songs`: the library has songs, so
/// "(single song)" can be chosen. `markers` are the concert markers of
/// the loaded playlist (gap indices into `rows`).
pub fn ui(
    ui: &mut Ui,
    playlists: &[String],
    selected: Option<&str>,
    single: bool,
    has_songs: bool,
    rows: &[SongRow],
    markers: SetMarkers,
) -> ListOutcome {
    let mut out = ListOutcome::default();
    ui.label(egui::RichText::new("Playlist").size(13.0).color(DIM));
    let width = ui.available_width() - 8.0;
    let shown = match (selected, single) {
        (Some(n), _) => n.to_string(),
        (None, true) => SINGLE_SONG.to_string(),
        (None, false) => "(none)".to_string(),
    };
    egui::ComboBox::from_id_source("rehearsal_playlist").width(width).selected_text(shown).show_ui(ui, |ui| {
        let on = single && selected.is_none();
        if (has_songs || on) && ui.selectable_label(on, SINGLE_SONG).clicked() && !on {
            out.library = true;
        }
        for name in playlists {
            if ui.selectable_label(selected == Some(name.as_str()), name).clicked() && selected != Some(name.as_str()) {
                out.select = Some(name.clone());
            }
        }
    });
    ui.add_space(8.0);
    if rows.is_empty() {
        let text = if playlists.is_empty() && !has_songs {
            "The library has no songs."
        } else {
            "Choose a playlist or (single song)."
        };
        ui.label(egui::RichText::new(text).size(13.0).color(DIM));
        return out;
    }
    for g in 0..=rows.len() {
        if markers.encore == Some(g) {
            marker_row(ui, "ENCORES");
        }
        if markers.end == Some(g) {
            marker_row(ui, "END OF CONCERT");
        }
        let Some(row) = rows.get(g) else { break };
        // after the end of the concert: still played, but shown dimmed
        let after_end = markers.end.map(|e| g >= e).unwrap_or(false);
        let (click, play) = song_row(ui, row, after_end);
        if click.is_some() {
            out.click = click;
        }
        if play.is_some() {
            out.play = play;
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

/// A marker row between two songs, tinted like in the playlist editor.
fn marker_row(ui: &mut Ui, text: &str) {
    let w = ui.available_width() - 8.0;
    let (rect, _) = ui.allocate_exact_size(vec2(w, 22.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 0.0, MARKER_FILL);
    p.text(pos2(rect.left() + 32.0, rect.center().y), Align2::LEFT_CENTER, text, FontId::proportional(12.5), ORANGE);
}

/// One row; returns its index (0-based) when clicked, and when
/// double-clicked (then not as a click). `after_end`: the song comes after
/// the end of the concert (dimmed).
fn song_row(ui: &mut Ui, row: &SongRow, after_end: bool) -> (Option<usize>, Option<usize>) {
    let w = ui.available_width() - 8.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 26.0), Sense::click());
    let p = ui.painter();
    if row.marked {
        p.rect_filled(rect, 3.0, Color32::from_rgba_unmultiplied(0xf0, 0x6b, 0x19, 0x70));
        p.rect_stroke(rect.shrink(0.5), 3.0, Stroke::new(1.0_f32, ORANGE));
    } else if resp.hovered() {
        p.rect_filled(rect, 3.0, PANEL);
    }
    if row.next && !row.marked {
        p.rect_stroke(rect.shrink(1.0), 3.0, Stroke::new(1.5_f32, ORANGE));
    }
    let color = match (&row.error, after_end && !row.marked && !row.current) {
        (Some(_), _) => RED,
        (None, true) => DIM,
        (None, false) => TEXT,
    };
    let mid = rect.center().y;
    p.text(pos2(rect.left() + 8.0, mid), Align2::LEFT_CENTER, row.number.to_string(), FontId::proportional(13.0), DIM);
    // right side first: length, MIDI symbol
    let mut right = rect.right() - 8.0;
    if !row.length.is_empty() {
        p.text(pos2(right, mid), Align2::RIGHT_CENTER, &row.length, FontId::proportional(13.0), color);
        right -= 40.0;
    }
    if row.midi {
        p.text(pos2(right, mid), Align2::RIGHT_CENTER, "\u{266a}", FontId::proportional(14.0), if row.marked { TEXT } else { ORANGE });
        right -= 18.0;
    }
    if row.mixer {
        let color = if row.marked { TEXT } else { ORANGE };
        crate::gui_mixer::paint_mixer_icon(p, pos2(right - 6.0, mid), 12.0, color);
        right -= 18.0;
    }
    // the name, cut to the space that is left
    let name_rect = egui::Rect::from_min_max(pos2(rect.left() + 32.0, rect.top()), pos2(right, rect.bottom()));
    p.with_clip_rect(name_rect).text(
        pos2(name_rect.left(), mid),
        Align2::LEFT_CENTER,
        if row.current {
            format!("\u{25b6} {}", row.name) // plays right now
        } else if row.next {
            format!("\u{2192} {}", row.name) // comes next
        } else {
            row.name.clone()
        },
        FontId::proportional(14.5),
        color,
    );
    p.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(0.5_f32, LINE));
    let resp = match &row.error {
        Some(e) => resp.on_hover_text(format!("Cannot be played, skipped: {}", e)),
        None => resp,
    };
    if row.error.is_some() {
        (None, None)
    } else if resp.double_clicked() {
        (None, Some(row.number - 1))
    } else if resp.clicked() {
        (Some(row.number - 1), None)
    } else {
        (None, None)
    }
}

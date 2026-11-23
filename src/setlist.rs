//! Concert planning for a playlist: where the encores start, where the
//! concert ends, and the running times with a pause between two songs.
//!
//! The markers live in a small file next to the .playlistprj
//! (`<name>.setlist`), so the .playlistprj stays exactly in uTool2's format.
//! Songs after the end of the concert stay in the playlist (exported and
//! played as usual); they only don't count for the concert length.

/// The two markers of a playlist. Each one is a gap index: the number of
/// songs before the marker (0 = before the first song, len = after the last).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetMarkers {
    /// the encores start here
    pub encore: Option<usize>,
    /// the concert ends here
    pub end: Option<usize>,
}

impl SetMarkers {
    pub fn is_empty(&self) -> bool {
        self.encore.is_none() && self.end.is_none()
    }

    /// Markers fitted to a playlist of `len` songs (it may have been changed
    /// elsewhere, e.g. with uTool2).
    pub fn clamp(self, len: usize) -> SetMarkers {
        SetMarkers { encore: self.encore.map(|g| g.min(len)), end: self.end.map(|g| g.min(len)) }
    }

    /// Read a .setlist file: `key = value` lines; unknown or broken lines
    /// are ignored.
    pub fn parse(text: &str) -> SetMarkers {
        let mut m = SetMarkers::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim().trim_matches('"').parse::<usize>().ok();
            match key.trim() {
                "encore" => m.encore = value,
                "end" => m.end = value,
                _ => {}
            }
        }
        m
    }

    /// The text of the .setlist file.
    pub fn generate(&self) -> String {
        let mut out = String::new();
        if let Some(g) = self.encore {
            out.push_str(&format!("encore = {}\r\n", g));
        }
        if let Some(g) = self.end {
            out.push_str(&format!("end = {}\r\n", g));
        }
        out
    }
}

/// Running times of a playlist.
#[derive(Clone, Debug, PartialEq)]
pub struct Timeline {
    /// per song: start and end in seconds from the start of the concert;
    /// None for songs after the end of the concert
    pub rows: Vec<Option<(f64, f64)>>,
    /// end of the main set (the last song before the encores); None without
    /// an encore marker inside the concert
    pub main_set: Option<f64>,
    /// end of the last song of the concert
    pub concert: f64,
    /// number of songs between the encore marker and the end of the concert
    pub encore_count: usize,
    /// number of songs of the concert
    pub song_count: usize,
}

/// Start and end of every song: the first song starts at 0, every other one
/// `pause` seconds after the end of the song before it. Markers take no time.
pub fn timeline(durations: &[f64], markers: &SetMarkers, pause: f64) -> Timeline {
    let m = markers.clamp(durations.len());
    let end = m.end.unwrap_or(durations.len());
    let mut rows = Vec::with_capacity(durations.len());
    let mut last_end: Option<f64> = None;
    let mut main_set = None;
    for (i, &d) in durations.iter().enumerate() {
        if m.encore == Some(i) && i <= end {
            main_set = Some(last_end.unwrap_or(0.0));
        }
        if i >= end {
            rows.push(None);
            continue;
        }
        let start = last_end.map(|e| e + pause).unwrap_or(0.0);
        let stop = start + d.max(0.0);
        rows.push(Some((start, stop)));
        last_end = Some(stop);
    }
    let concert = last_end.unwrap_or(0.0);
    if m.encore == Some(durations.len()) && durations.len() <= end {
        main_set = Some(concert);
    }
    let encore_count = match m.encore {
        Some(g) if g < end => end - g,
        _ => 0,
    };
    Timeline { rows, main_set, concert, encore_count, song_count: end }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the song lengths of the Excel sheet the editor replaces
    const SHEET: [f64; 17] = [
        220.0, 165.0, 170.0, 262.0, 203.0, 245.0, 225.0, 221.0, 250.0, 285.0, 187.0, 210.0, 192.0, 240.0, 155.0,
        220.0, 240.0,
    ];

    #[test]
    fn matches_the_excel_sheet() {
        let m = SetMarkers { encore: Some(12), end: None };
        let t = timeline(&SHEET, &m, 60.0);
        assert_eq!(t.rows[0], Some((0.0, 220.0))); // 0:00:00 - 0:03:40
        assert_eq!(t.rows[1], Some((280.0, 445.0))); // 0:04:40 - 0:07:25
        assert_eq!(t.rows[11].unwrap().1, 3303.0); // 0:55:03
        assert_eq!(t.main_set, Some(3303.0));
        assert_eq!(t.rows[12].unwrap().0, 3363.0); // 0:56:03: one pause across the marker
        assert_eq!(t.concert, 4650.0); // 1:17:30
        assert_eq!(t.encore_count, 5);
        assert_eq!(t.song_count, 17);
    }

    #[test]
    fn songs_after_the_end_have_no_times() {
        let m = SetMarkers { encore: Some(12), end: Some(15) };
        let t = timeline(&SHEET, &m, 60.0);
        assert_eq!(t.concert, t.rows[14].unwrap().1);
        assert_eq!(t.rows[15], None);
        assert_eq!(t.rows[16], None);
        assert_eq!(t.encore_count, 3);
        assert_eq!(t.song_count, 15);
    }

    #[test]
    fn without_markers_and_empty() {
        let t = timeline(&[60.0, 60.0], &SetMarkers::default(), 30.0);
        assert_eq!(t.concert, 150.0);
        assert_eq!(t.main_set, None);
        assert_eq!(t.encore_count, 0);
        let t = timeline(&[], &SetMarkers { encore: Some(3), end: Some(5) }, 60.0);
        assert_eq!(t.concert, 0.0);
        assert!(t.rows.is_empty());
    }

    #[test]
    fn encore_marker_after_the_end_is_ignored() {
        let m = SetMarkers { encore: Some(2), end: Some(1) };
        let t = timeline(&[10.0, 10.0, 10.0], &m, 5.0);
        assert_eq!(t.main_set, None);
        assert_eq!(t.encore_count, 0);
        assert_eq!(t.concert, 10.0);
    }

    #[test]
    fn file_roundtrip() {
        let m = SetMarkers { encore: Some(12), end: Some(17) };
        assert_eq!(SetMarkers::parse(&m.generate()), m);
        assert_eq!(SetMarkers::parse("junk\nend = x\nencore=3"), SetMarkers { encore: Some(3), end: None });
        assert!(SetMarkers::default().generate().is_empty());
    }
}

//! Renders a song into the stereo backup MP3: every track goes to its
//! LP-16 output, each output adds to the left and the right side with the
//! gains of the backup mix (`crate::backup_mix`) times the song mix of the
//! song (`crate::mix::song_gains`, the uTool2 mixer values, like in the
//! rehearsal player). Samples beyond full scale are clipped at +-1.0 and
//! counted, so the report can warn.
//!
//! Encoding: LAME, CBR 192 kbit/s, quality 0 (best), plain stereo (not
//! joint stereo: the two sides are unrelated mono mixes, the click must not
//! leak into the backing side). In front of the audio comes an ID3v2.3 tag
//! (title, album, track). The file is written atomically (`write_atomic`).

use crate::backup_mix::BackupMix;
use crate::mix::{song_gains, STRIPS};
use crate::{open_song_tracks, write_atomic, SongProject, WavReader};
use mp3lame_encoder::{max_required_buffer_size, Bitrate, Builder, DualPcm, FlushNoGap, Mode, Quality};
use std::io::Write;
use std::path::Path;

/// Frames read, mixed and encoded at once.
const BLOCK: usize = 4096;

/// The ID3 tag of one MP3.
#[derive(Clone, Debug, Default)]
pub struct Tags {
    pub title: String,
    pub album: String,
    /// position in the playlist (1-based) and its length; 0 = no track
    pub track: usize,
    pub total: usize,
}

/// What happened while one song was rendered.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SongReport {
    pub seconds: f64,
    /// highest sample before clipping, in dBFS (-inf for silence)
    pub peak_db: f32,
    /// samples (left and right counted separately) clipped at +-1.0
    pub clipped: u64,
    /// tracks left out, MIDI not included, nothing mixed, ...
    pub warnings: Vec<String>,
}

/// The mixing part, without encoding: reads all tracks block by block and
/// sums them into left and right.
pub struct Mixdown {
    tracks: Vec<(usize, WavReader)>,
    left_gain: [f32; STRIPS],
    right_gain: [f32; STRIPS],
    rate: u32,
    frames: u64,
    done: u64,
    scratch: Vec<f32>,
    peak: f32,
    clipped: u64,
    warnings: Vec<String>,
}

impl Mixdown {
    /// Open the tracks of `project` with the gains of `mix` x song mix.
    pub fn new(project: &SongProject, mix: &BackupMix) -> Mixdown {
        let opened = open_song_tracks(project);
        let (l, r) = mix.gains();
        let song = song_gains(project);
        let left_gain: [f32; STRIPS] = std::array::from_fn(|k| l[k] * song[k]);
        let right_gain: [f32; STRIPS] = std::array::from_fn(|k| r[k] * song[k]);
        let mut warnings = opened.warnings;
        if project.midi_path().is_some() {
            warnings.push("The MIDI file of the song is not part of the MP3".to_string());
        }
        // the length of the song: the longest track, also the silent ones
        let frames = opened.tracks.iter().map(|(_, r)| r.frames()).max().unwrap_or(0);
        // tracks that reach neither side need not be read
        let tracks: Vec<_> =
            opened.tracks.into_iter().filter(|(k, _)| left_gain[*k] > 0.0 || right_gain[*k] > 0.0).collect();
        if tracks.is_empty() {
            warnings.push("No track reaches the left or the right side: the MP3 is silent".to_string());
        }
        Mixdown {
            tracks,
            left_gain,
            right_gain,
            rate: opened.rate,
            frames,
            done: 0,
            scratch: vec![0.0; BLOCK],
            peak: 0.0,
            clipped: 0,
            warnings,
        }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Mix the next block into `left` and `right` (resized to the frames
    /// of the block). Returns the number of frames, 0 at the end.
    pub fn next_block(&mut self, left: &mut Vec<f32>, right: &mut Vec<f32>) -> usize {
        let n = (self.frames - self.done).min(BLOCK as u64) as usize;
        left.clear();
        left.resize(n, 0.0);
        right.clear();
        right.resize(n, 0.0);
        if n == 0 {
            return 0;
        }
        for (k, reader) in self.tracks.iter_mut() {
            let got = reader.read(&mut self.scratch[..n]);
            let (gl, gr) = (self.left_gain[*k], self.right_gain[*k]);
            for (i, s) in self.scratch[..got].iter().enumerate() {
                left[i] += s * gl;
                right[i] += s * gr;
            }
        }
        for s in left.iter_mut().chain(right.iter_mut()) {
            let a = s.abs();
            if a > self.peak {
                self.peak = a;
            }
            if a > 1.0 {
                self.clipped += 1;
                *s = s.clamp(-1.0, 1.0);
            }
        }
        self.done += n as u64;
        n
    }

    /// Frames mixed so far.
    pub fn position(&self) -> u64 {
        self.done
    }

    fn report(self) -> SongReport {
        SongReport {
            seconds: if self.rate > 0 { self.frames as f64 / self.rate as f64 } else { 0.0 },
            peak_db: if self.peak > 0.0 { 20.0 * self.peak.log10() } else { f32::NEG_INFINITY },
            clipped: self.clipped,
            warnings: self.warnings,
        }
    }
}

/// The MP3 sample rate for a song rate: MPEG-1 has 32, 44.1 and 48 kHz;
/// other rates are resampled by LAME.
fn mp3_rate(rate: u32) -> u32 {
    match rate {
        32000 | 44100 | 48000 => rate,
        r if r % 11025 == 0 => 44100,
        _ => 48000,
    }
}

/// Render `project` into the MP3 `dest`. `progress` gets the fraction done
/// (0..1) after every block; when it returns false the export stops, no
/// file is left behind and the error kind is `Interrupted`.
pub fn render_song(
    project: &SongProject,
    mix: &BackupMix,
    tags: &Tags,
    dest: &Path,
    progress: &mut dyn FnMut(f64) -> bool,
) -> std::io::Result<SongReport> {
    let mut mixdown = Mixdown::new(project, mix);
    if mixdown.rate() == 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "the song has no sample rate"));
    }
    let mut encoder = encoder(mixdown.rate())?;
    let total = mixdown.frames().max(1) as f64;
    let (mut left, mut right) = (Vec::with_capacity(BLOCK), Vec::with_capacity(BLOCK));
    let mut mp3 = Vec::with_capacity(max_required_buffer_size(BLOCK));
    write_atomic(dest, |w| {
        w.write_all(&id3_tag(tags))?;
        loop {
            let n = mixdown.next_block(&mut left, &mut right);
            if n == 0 {
                break;
            }
            mp3.clear();
            encoder.encode_to_vec(DualPcm { left: &left, right: &right }, &mut mp3).map_err(lame_error)?;
            w.write_all(&mp3)?;
            if !progress(mixdown.position() as f64 / total) {
                return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
            }
        }
        mp3.clear();
        mp3.reserve(7200);
        encoder.flush_to_vec::<FlushNoGap>(&mut mp3).map_err(lame_error)?;
        w.write_all(&mp3)
    })?;
    Ok(mixdown.report())
}

fn lame_error(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(format!("MP3 encoder: {}", e))
}

/// LAME: stereo, CBR 192 kbit/s, quality 0, no VBR/LAME info frame.
fn encoder(rate: u32) -> std::io::Result<mp3lame_encoder::Encoder> {
    let mut b = Builder::new().ok_or_else(|| lame_error("cannot be created"))?;
    b.set_num_channels(2).map_err(lame_error)?;
    b.set_sample_rate(rate).map_err(lame_error)?;
    b.set_output_sample_rate(std::num::NonZeroU32::new(mp3_rate(rate))).map_err(lame_error)?;
    b.set_vbr_mode(mp3lame_encoder::VbrMode::Off).map_err(lame_error)?;
    b.set_brate(Bitrate::Kbps192).map_err(lame_error)?;
    b.set_mode(Mode::Stereo).map_err(lame_error)?;
    b.set_quality(Quality::Best).map_err(lame_error)?;
    b.set_to_write_vbr_tag(false).map_err(lame_error)?;
    b.build().map_err(lame_error)
}

/// An ID3v2.3 tag with TIT2, TALB and TRCK (empty values are left out).
/// Texts are UTF-16 with BOM, so umlauts survive on every player.
pub fn id3_tag(tags: &Tags) -> Vec<u8> {
    fn text_frame(id: &[u8; 4], text: &str) -> Vec<u8> {
        let mut body = vec![1u8, 0xFF, 0xFE];
        for u in text.encode_utf16() {
            body.extend_from_slice(&u.to_le_bytes());
        }
        let mut f = Vec::with_capacity(10 + body.len());
        f.extend_from_slice(id);
        f.extend_from_slice(&(body.len() as u32).to_be_bytes());
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(&body);
        f
    }
    let mut frames = Vec::new();
    if !tags.title.is_empty() {
        frames.extend(text_frame(b"TIT2", &tags.title));
    }
    if !tags.album.is_empty() {
        frames.extend(text_frame(b"TALB", &tags.album));
    }
    if tags.track > 0 {
        let t = if tags.total > 0 { format!("{}/{}", tags.track, tags.total) } else { tags.track.to_string() };
        frames.extend(text_frame(b"TRCK", &t));
    }
    if frames.is_empty() {
        return Vec::new();
    }
    let size = frames.len() as u32;
    let mut tag = b"ID3\x03\x00\x00".to_vec();
    // the size is "syncsafe": 7 bits per byte
    tag.extend_from_slice(&[(size >> 21 & 0x7F) as u8, (size >> 14 & 0x7F) as u8, (size >> 7 & 0x7F) as u8, (size & 0x7F) as u8]);
    tag.extend(frames);
    tag
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::temp_dir;
    use crate::{generate_songprj, ProjectTrack};
    use std::fs;

    const RATE: u32 = 48000;

    /// 16 bit mono WAV with a constant value
    fn wav(dir: &Path, name: &str, rate: u32, frames: usize, value: f32) -> String {
        let s = (value * 32767.0) as i16;
        let data_len = frames * 2;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2).to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data_len as u32).to_le_bytes());
        for _ in 0..frames {
            b.extend_from_slice(&s.to_le_bytes());
        }
        let p = dir.join(format!("{}.wav", name));
        fs::write(&p, b).unwrap();
        p.to_str().unwrap().to_string()
    }

    /// a song with one track per WAV, track i on output i+1
    fn song(dir: &Path, wavs: &[&str]) -> SongProject {
        let audio_lib = dir.to_str().unwrap();
        let mut prj = SongProject::parse(&generate_songprj("Test", 0, RATE, 16, 1), audio_lib);
        prj.tracks = wavs.iter().enumerate().map(|(i, w)| ProjectTrack::new(i as i32, w, audio_lib)).collect();
        prj
    }

    fn mixdown_all(m: &mut Mixdown) -> (Vec<f32>, Vec<f32>) {
        let (mut l, mut r, mut al, mut ar) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        while m.next_block(&mut l, &mut r) > 0 {
            al.extend_from_slice(&l);
            ar.extend_from_slice(&r);
        }
        (al, ar)
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn outputs_go_to_their_side() {
        let dir = temp_dir("backup-sides");
        let a = wav(&dir, "backing", RATE, 10000, 0.25);
        let b = wav(&dir, "click", RATE, 6000, 0.5);
        let c = wav(&dir, "nowhere", RATE, 12000, 0.5);
        let prj = song(&dir, &[&a, &b, &c]);
        let mut mix = BackupMix::default();
        mix.left.set_level(0, 0.0);
        mix.right.set_level(1, -6.0);
        let mut m = Mixdown::new(&prj, &mix);
        assert_eq!(m.frames(), 12000, "the longest track, also a silent one");
        let (l, r) = mixdown_all(&mut m);
        assert_eq!(l.len(), 12000);
        assert!(close(l[0], 0.25) && close(r[0], 0.5 * 0.501187));
        assert!(close(l[7000], 0.25) && r[7000] == 0.0, "the click track has ended");
        assert!(l[11000] == 0.0 && r[11000] == 0.0, "output 3 goes nowhere");
        let rep = m.report();
        assert_eq!(rep.clipped, 0);
        assert!((rep.peak_db + 12.0).abs() < 0.1, "{}", rep.peak_db);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_song_mix_counts() {
        let dir = temp_dir("backup-songmix");
        let a = wav(&dir, "a", RATE, 100, 0.25);
        let b = wav(&dir, "b", RATE, 100, 0.25);
        let mut prj = song(&dir, &[&a, &b]);
        prj.tracks[0].monitor_level = "-6.0".into();
        prj.tracks[1].monitor_mute = "1".into();
        let mut mix = BackupMix::default();
        mix.left.set_level(0, 0.0);
        mix.left.set_level(1, 0.0);
        let (l, _) = mixdown_all(&mut Mixdown::new(&prj, &mix));
        assert!(close(l[0], 0.25 * 0.501187), "{}", l[0]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn overload_is_clipped_and_counted() {
        let dir = temp_dir("backup-clip");
        let a = wav(&dir, "a", RATE, 50, 0.8);
        let b = wav(&dir, "b", RATE, 50, 0.8);
        let prj = song(&dir, &[&a, &b]);
        let mut mix = BackupMix::default();
        mix.left.set_level(0, 0.0);
        mix.left.set_level(1, 0.0);
        let mut m = Mixdown::new(&prj, &mix);
        let (l, r) = mixdown_all(&mut m);
        assert!(l.iter().all(|s| *s == 1.0) && r.iter().all(|s| *s == 0.0));
        let rep = m.report();
        assert_eq!(rep.clipped, 50);
        assert!(close(rep.peak_db, 20.0 * 1.6f32.log10()), "{}", rep.peak_db);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn warnings_for_left_out_tracks_midi_and_silence() {
        let dir = temp_dir("backup-warn");
        let a = wav(&dir, "a", RATE, 100, 0.1);
        let b = wav(&dir, "b", 44100, 100, 0.1);
        let mut prj = song(&dir, &[&a, &b]);
        prj.midi_file = "x.mid".into();
        let rep = Mixdown::new(&prj, &BackupMix::default()).report();
        let all = rep.warnings.join("\n");
        assert!(all.contains("sample rate 44100"), "{}", all);
        assert!(all.contains("MIDI"), "{}", all);
        assert!(all.contains("silent"), "{}", all);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn id3_tag_layout() {
        let t = id3_tag(&Tags { title: "Gr\u{fc}n".into(), album: "Tour".into(), track: 3, total: 12 });
        assert_eq!(&t[0..6], b"ID3\x03\x00\x00");
        let size = ((t[6] as usize) << 21) | ((t[7] as usize) << 14) | ((t[8] as usize) << 7) | t[9] as usize;
        assert_eq!(size, t.len() - 10);
        assert_eq!(&t[10..14], b"TIT2");
        // encoding 1, BOM, "Grün" in UTF-16LE
        assert_eq!(&t[20..23], &[1, 0xFF, 0xFE]);
        assert_eq!(&t[23..31], &[b'G', 0, b'r', 0, 0xFC, 0, b'n', 0]);
        let s = String::from_utf8_lossy(&t);
        assert!(s.contains("TALB") && s.contains("TRCK"));
        assert!(id3_tag(&Tags::default()).is_empty());
    }

    #[test]
    fn mp3_is_plain_stereo_192_kbit() {
        let dir = temp_dir("backup-mp3");
        let a = wav(&dir, "a", RATE, RATE as usize, 0.3);
        let prj = song(&dir, &[&a]);
        let mut mix = BackupMix::default();
        mix.left.set_level(0, 0.0);
        let dest = dir.join("out.mp3");
        let tags = Tags { title: "Test".into(), album: "P".into(), track: 1, total: 1 };
        let mut calls = 0;
        let rep = render_song(&prj, &mix, &tags, &dest, &mut |f| {
            calls += 1;
            assert!((0.0..=1.0).contains(&f));
            true
        })
        .unwrap();
        assert!(calls > 0);
        assert!((rep.seconds - 1.0).abs() < 1e-9);
        let b = fs::read(&dest).unwrap();
        let tag_len = 10 + (((b[6] as usize) << 21) | ((b[7] as usize) << 14) | ((b[8] as usize) << 7) | b[9] as usize);
        let h = &b[tag_len..tag_len + 4];
        assert_eq!(h[0], 0xFF, "frame sync");
        assert_eq!(h[1] & 0xFE, 0xFA, "MPEG-1 Layer III");
        assert_eq!(h[2] >> 4, 11, "bitrate index 11 = 192 kbit/s");
        assert_eq!((h[2] >> 2) & 3, 1, "48 kHz");
        assert_eq!(h[3] >> 6, 0, "mode 00 = stereo, not joint stereo");
        // about 1 s at 192 kbit/s
        let audio = b.len() - tag_len;
        assert!((20_000..30_000).contains(&audio), "{}", audio);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_leaves_no_file() {
        let dir = temp_dir("backup-cancel");
        let a = wav(&dir, "a", RATE, RATE as usize, 0.3);
        let prj = song(&dir, &[&a]);
        let dest = dir.join("out.mp3");
        let err = render_song(&prj, &BackupMix::default(), &Tags::default(), &dest, &mut |_| false).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
        assert!(!dest.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().filter(|e| e.as_ref().unwrap().path().extension().map(|x| x == "lp16tmp").unwrap_or(false)).count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mp3_rates() {
        assert_eq!(mp3_rate(44100), 44100);
        assert_eq!(mp3_rate(48000), 48000);
        assert_eq!(mp3_rate(88200), 44100);
        assert_eq!(mp3_rate(96000), 48000);
    }
}

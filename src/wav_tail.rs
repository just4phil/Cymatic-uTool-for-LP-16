//! Find WAVs whose end is not clean (a burst of digital clipping, or a hard
//! cut), and repair them with a short fade-out, optionally after cutting off
//! the broken end. Some DAW exports (seen with Cakewalk) end with a few ms of
//! full-scale garbage, which the LP-16 plays as a loud click.

use crate::{copy_atomic, write_atomic, SampleFormat, WavInfo, WavReader};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// How much of the end is looked at.
pub const TAIL_WINDOW_MS: f64 = 200.0;
/// A sample at or above this level (linear, 1.0 = full scale) counts as clipped.
pub const CLIP_LEVEL: f32 = 0.99;
/// Clipped samples up to this far apart belong to one clipped region.
pub const CLIP_GAP_MS: f64 = 1.0;
/// Below this level (dBFS) the end counts as silent.
pub const SILENCE_DB: f32 = -60.0;
/// The last bit of sound before the end (or before the trailing silence).
pub const END_WINDOW_MS: f64 = 0.5;
/// If that last bit is louder than this (dBFS), the sound stops abruptly.
/// A `FADE_MS` fade-out of a full-scale signal ends at about -40 dBFS here.
pub const ABRUPT_DB: f32 = -36.0;
/// Cut this much more than the broken end, so the fade starts on clean audio.
pub const TRIM_MARGIN_MS: f64 = 2.0;
/// Length of the repair fade-out.
pub const FADE_MS: f64 = 50.0;

/// What is wrong with the end of a WAV.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TailReport {
    /// length of the clipped region at the end (0 = no clipping)
    pub clipped_ms: f64,
    /// silence after the sound (after the clipped region, if any)
    pub silence_ms: f64,
    /// peak of the last `END_WINDOW_MS` of sound before the clipped region / silence, dBFS
    pub end_level_db: f32,
    /// how much to cut so that only clean audio remains, in whole ms
    pub suggested_trim_ms: f64,
}

impl TailReport {
    /// One line for a tooltip, e.g. "Clipping in the last 12 ms".
    pub fn describe(&self) -> String {
        let mut s = if self.clipped_ms > 0.0 {
            format!("Clipping in the last {:.0} ms", self.clipped_ms.max(1.0))
        } else {
            format!("Ends abruptly at {:.0} dBFS", self.end_level_db)
        };
        if self.silence_ms >= 1.0 {
            s += &format!(", followed by {:.0} ms of silence", self.silence_ms);
        }
        s
    }
}

fn db(level: f32) -> f32 {
    if level <= 1e-6 {
        -120.0
    } else {
        20.0 * level.log10()
    }
}

fn from_db(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn ms_to_frames(ms: f64, rate: u32) -> u64 {
    (ms * rate as f64 / 1000.0).round() as u64
}

fn frames_to_ms(frames: u64, rate: u32) -> f64 {
    frames as f64 * 1000.0 / rate as f64
}

/// Look at the last `TAIL_WINDOW_MS` of `path`. `None` = the WAV ends
/// cleanly (fades out to silence, or the whole window is silent).
pub fn analyze_tail(path: &str) -> io::Result<Option<TailReport>> {
    let mut r = WavReader::open(path)?;
    let rate = r.rate();
    let ch = r.channels() as usize;
    let total = r.frames();
    let w = ms_to_frames(TAIL_WINDOW_MS, rate).min(total) as usize;
    if w == 0 {
        return Ok(None);
    }
    r.seek(total - w as u64)?;
    let mut buf = vec![0f32; w * ch];
    let got = r.read(&mut buf);
    // peak of every frame over all channels
    let peaks: Vec<f32> = buf[..got * ch]
        .chunks_exact(ch)
        .map(|f| f.iter().fold(0f32, |m, x| m.max(x.abs())))
        .collect();
    Ok(analyze_peaks(&peaks, rate))
}

/// The analysis on per-frame peaks (the end of the file is the end of `peaks`).
fn analyze_peaks(peaks: &[f32], rate: u32) -> Option<TailReport> {
    let w = peaks.len();
    let silence = from_db(SILENCE_DB);
    // end of the sound: after the last frame above the silence level
    let end = peaks.iter().rposition(|&p| p > silence)? + 1;
    // clipped region: clipped frames reaching up to the end of the sound,
    // with gaps of at most CLIP_GAP_MS
    let gap = ms_to_frames(CLIP_GAP_MS, rate).max(1) as usize;
    let mut clip_start = end;
    let mut i = end;
    while i > 0 {
        let lo = i.saturating_sub(gap + 1);
        match (lo..i).rev().find(|&j| peaks[j] >= CLIP_LEVEL) {
            Some(j) => {
                clip_start = j;
                i = j;
            }
            None => break,
        }
    }
    let sound_end = clip_start; // last clean frame + 1
    let e = ms_to_frames(END_WINDOW_MS, rate).max(1) as usize;
    let end_level = peaks[sound_end.saturating_sub(e)..sound_end].iter().fold(0f32, |m, &p| m.max(p));
    let clipped = end - clip_start;
    if clipped == 0 && db(end_level) <= ABRUPT_DB {
        return None;
    }
    let bad = w - sound_end; // clipped region + silence behind it
    let suggested_trim_ms = if bad == 0 { 0.0 } else { (frames_to_ms(bad as u64, rate) + TRIM_MARGIN_MS).ceil() };
    Some(TailReport {
        clipped_ms: frames_to_ms(clipped as u64, rate),
        silence_ms: frames_to_ms((w - end) as u64, rate),
        end_level_db: db(end_level),
        suggested_trim_ms,
    })
}

/// File the original is kept in: "x.wav" -> "x.orig.wav" in the same folder.
pub fn backup_path(path: &str) -> PathBuf {
    let p = Path::new(path);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    p.with_file_name(format!("{}.orig.wav", stem))
}

/// Is this a backup written by `fix_tail` ("*.orig.wav")? Such files are
/// never offered as tracks.
pub fn is_backup_wav(path: impl AsRef<Path>) -> bool {
    path.as_ref()
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase().ends_with(".orig.wav"))
        .unwrap_or(false)
}

/// Cut `trim_ms` off the end of `path` and fade the new end out over
/// `fade_ms` (linear, down to exactly 0 on the last frame). The audio before
/// the fade and all other chunks are copied unchanged; the file is replaced
/// atomically. Before the first change the original is copied to
/// `backup_path` (an existing backup is never overwritten).
pub fn fix_tail(path: &str, trim_ms: f64, fade_ms: f64) -> io::Result<()> {
    // validates the format like the player does
    let reader = WavReader::open(path)?;
    let info: WavInfo = *reader.info();
    drop(reader);
    let fb = info.channels as u64 * (info.bits as u64 / 8);
    let frames = info.data_len / fb;
    let trim = ms_to_frames(trim_ms.max(0.0), info.rate);
    let fade = ms_to_frames(fade_ms.max(0.0), info.rate).max(1);
    if frames < trim + fade + 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{}: too short for a {:.0} ms cut and a {:.0} ms fade-out", path, trim_ms, fade_ms),
        ));
    }
    let new_frames = frames - trim;
    let new_len = new_frames * fb;
    if new_len > u32::MAX as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{}: too large", path)));
    }

    let backup = backup_path(path);
    if !backup.exists() {
        copy_atomic(path, &backup)?;
    }

    let mut src = fs::File::open(path)?;
    let file_len = src.metadata()?.len();
    let old_end = info.data_off + info.data_len + info.data_len % 2; // incl. pad byte
    let after = file_len.saturating_sub(old_end); // chunks behind "data"
    let new_file_len = info.data_off + new_len + new_len % 2 + after;

    // header up to the data chunk header, RIFF size patched
    let mut head = vec![0u8; (info.data_off - 8) as usize];
    src.read_exact(&mut head)?;
    head[4..8].copy_from_slice(&((new_file_len - 8) as u32).to_le_bytes());

    // the fade region, decoded and faded in memory (a few KB)
    let fade_start = new_frames - fade;
    let mut tail = vec![0u8; (fade * fb) as usize];
    src.seek(SeekFrom::Start(info.data_off + fade_start * fb))?;
    src.read_exact(&mut tail)?;
    apply_fade(&mut tail, &info);

    let mut rest = Vec::new();
    if after > 0 {
        src.seek(SeekFrom::Start(old_end))?;
        src.read_to_end(&mut rest)?;
    }

    drop(src); // no open handle while the new file is renamed over it
    write_atomic(Path::new(path), |w| {
        let mut input = fs::File::open(path)?;
        w.write_all(&head)?;
        w.write_all(b"data")?;
        w.write_all(&(new_len as u32).to_le_bytes())?;
        input.seek(SeekFrom::Start(info.data_off))?;
        let n = io::copy(&mut (&mut input).take(fade_start * fb), w)?;
        if n != fade_start * fb {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "WAV shorter than expected"));
        }
        w.write_all(&tail)?;
        if new_len % 2 == 1 {
            w.write_all(&[0])?;
        }
        w.write_all(&rest)
    })
}

/// Fade `bytes` (whole frames) linearly from 1 to 0 on the last frame.
/// Integer samples are scaled as integers, so nothing but the gain changes.
fn apply_fade(bytes: &mut [u8], info: &WavInfo) {
    let sb = info.bits as usize / 8;
    let fb = sb * info.channels as usize;
    let n = bytes.len() / fb;
    for (k, frame) in bytes.chunks_exact_mut(fb).enumerate() {
        let g = 1.0 - (k + 1) as f64 / n as f64;
        for b in frame.chunks_exact_mut(sb) {
            match (info.format, sb) {
                (SampleFormat::Float, 4) => {
                    let v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64 * g;
                    b.copy_from_slice(&(v as f32).to_le_bytes());
                }
                (_, 1) => {
                    let v = (b[0] as f64 - 128.0) * g;
                    b[0] = (v.round() + 128.0).clamp(0.0, 255.0) as u8;
                }
                (_, 2) => {
                    let v = i16::from_le_bytes([b[0], b[1]]) as f64 * g;
                    b.copy_from_slice(&(v.round() as i16).to_le_bytes());
                }
                (_, 3) => {
                    let v = (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f64 * g;
                    b.copy_from_slice(&(v.round() as i32).to_le_bytes()[..3]);
                }
                _ => {
                    let v = i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64 * g;
                    b.copy_from_slice(&(v.round() as i32).to_le_bytes());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::temp_dir;

    const RATE: u32 = 48000;

    /// WAV with a LIST chunk before and an "id3 " chunk after the data.
    fn wav_bytes(fmt_tag: u16, bits: u16, channels: u16, samples: &[f64]) -> Vec<u8> {
        let sb = bits as usize / 8;
        let mut data = Vec::new();
        for &v in samples {
            match (fmt_tag, sb) {
                (3, _) => data.extend_from_slice(&(v as f32).to_le_bytes()),
                (_, 1) => data.push((v * 127.0 + 128.0).round() as u8),
                (_, 2) => data.extend_from_slice(&((v * 32767.0).round() as i16).to_le_bytes()),
                (_, 3) => data.extend_from_slice(&((v * 8388607.0).round() as i32).to_le_bytes()[..3]),
                _ => data.extend_from_slice(&((v * 2147483647.0).round() as i32).to_le_bytes()),
            }
        }
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&fmt_tag.to_le_bytes());
        fmt.extend_from_slice(&channels.to_le_bytes());
        fmt.extend_from_slice(&RATE.to_le_bytes());
        fmt.extend_from_slice(&(RATE * channels as u32 * sb as u32).to_le_bytes());
        fmt.extend_from_slice(&(channels * sb as u16).to_le_bytes());
        fmt.extend_from_slice(&bits.to_le_bytes());
        let mut body = b"WAVE".to_vec();
        for (id, b) in [(&b"fmt "[..], &fmt[..]), (b"LIST", b"INFOabc"), (b"data", &data[..]), (b"id3 ", b"tag!")] {
            body.extend_from_slice(id);
            body.extend_from_slice(&(b.len() as u32).to_le_bytes());
            body.extend_from_slice(b);
            if b.len() % 2 == 1 {
                body.push(0);
            }
        }
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    fn sine(frames: usize, amp: f64) -> Vec<f64> {
        (0..frames).map(|i| amp * (i as f64 * 2.0 * std::f64::consts::PI * 440.0 / RATE as f64).sin()).collect()
    }

    fn write(dir: &Path, name: &str, bytes: Vec<u8>) -> String {
        let p = dir.join(name);
        fs::write(&p, bytes).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn read_all(path: &str) -> Vec<f32> {
        let mut r = WavReader::open(path).unwrap();
        let mut v = vec![0f32; r.frames() as usize * r.channels() as usize];
        let n = r.read(&mut v);
        v.truncate(n * r.channels() as usize);
        v
    }

    #[test]
    fn clean_fade_is_not_reported() {
        let dir = temp_dir("tail_clean");
        let n = RATE as usize;
        let s: Vec<f64> = sine(n, 0.8).iter().enumerate().map(|(i, v)| v * (1.0 - i as f64 / n as f64)).collect();
        let p = write(&dir, "clean.wav", wav_bytes(1, 16, 1, &s));
        assert_eq!(analyze_tail(&p).unwrap(), None);
        // digital silence at the end is fine, too
        let mut s2 = s.clone();
        s2.extend(std::iter::repeat(0.0).take(4800));
        let p = write(&dir, "silence.wav", wav_bytes(1, 24, 1, &s2));
        assert_eq!(analyze_tail(&p).unwrap(), None);
        // a file that is completely silent
        let p = write(&dir, "zero.wav", wav_bytes(1, 16, 1, &[0.0; 100]));
        assert_eq!(analyze_tail(&p).unwrap(), None);
    }

    #[test]
    fn abrupt_end_is_reported_without_trim() {
        let dir = temp_dir("tail_abrupt");
        let p = write(&dir, "cut.wav", wav_bytes(1, 16, 1, &sine(RATE as usize, 0.5)));
        let r = analyze_tail(&p).unwrap().expect("abrupt end");
        assert_eq!(r.clipped_ms, 0.0);
        assert_eq!(r.suggested_trim_ms, 0.0);
        assert!(r.end_level_db > -7.0 && r.end_level_db < -5.0, "{:?}", r);
        assert!(r.describe().starts_with("Ends abruptly"));
    }

    #[test]
    fn abrupt_end_before_silence_suggests_cutting_the_silence() {
        let dir = temp_dir("tail_abrupt_silence");
        let mut s = sine(RATE as usize, 0.5);
        s.extend(std::iter::repeat(0.0).take(480)); // 10 ms
        let p = write(&dir, "cut.wav", wav_bytes(1, 16, 1, &s));
        let r = analyze_tail(&p).unwrap().expect("abrupt end");
        assert!((r.silence_ms - 10.0).abs() < 0.1, "{:?}", r);
        assert_eq!(r.suggested_trim_ms, 12.0);
    }

    #[test]
    fn clipped_burst_at_the_end_is_reported() {
        let dir = temp_dir("tail_clip");
        let n = RATE as usize;
        let mut s: Vec<f64> = sine(n, 0.8).iter().enumerate().map(|(i, v)| v * (1.0 - i as f64 / n as f64)).collect();
        // 10 ms of full scale garbage, alternating
        s.extend((0..480).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }));
        for (tag, bits) in [(1u16, 16u16), (1, 24), (1, 32), (3, 32), (1, 8)] {
            let p = write(&dir, &format!("clip{}_{}.wav", tag, bits), wav_bytes(tag, bits, 1, &s));
            let r = analyze_tail(&p).unwrap().expect("clipped end");
            assert!((r.clipped_ms - 10.0).abs() < 0.1, "{} {}: {:?}", tag, bits, r);
            assert_eq!(r.suggested_trim_ms, 12.0);
            assert!(r.describe().starts_with("Clipping in the last 10 ms"));
        }
    }

    #[test]
    fn fix_trims_fades_and_keeps_everything_else() {
        let dir = temp_dir("tail_fix");
        let n = RATE as usize / 2;
        let mut s = sine(n * 2, 0.5); // stereo, interleaved
        s.extend((0..960).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })); // 10 ms stereo
        for (tag, bits) in [(1u16, 8u16), (1, 16), (1, 24), (1, 32), (3, 32)] {
            let name = format!("fix{}_{}.wav", tag, bits);
            let p = write(&dir, &name, wav_bytes(tag, bits, 2, &s));
            let original = fs::read(&p).unwrap();
            let before = WavInfo::read(&p).unwrap();
            let r = analyze_tail(&p).unwrap().expect("clipped");
            fix_tail(&p, r.suggested_trim_ms, FADE_MS).unwrap();

            let after = WavInfo::read(&p).unwrap();
            let fb = 2 * bits as u64 / 8;
            let trimmed = ms_to_frames(12.0, RATE);
            assert_eq!(after.data_len, before.data_len - trimmed * fb, "{}", name);
            let bytes = fs::read(&p).unwrap();
            // RIFF size, chunks before and after the data kept
            assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize, bytes.len() - 8);
            assert_eq!(&bytes[..after.data_off as usize - 8], {
                let mut h = original[..before.data_off as usize - 8].to_vec();
                h[4..8].copy_from_slice(&bytes[4..8]);
                h
            });
            assert!(bytes.ends_with(b"id3 \x04\x00\x00\x00tag!"), "{}", name);
            // audio before the fade unchanged byte for byte
            let fade = ms_to_frames(FADE_MS, RATE);
            let keep = ((after.data_len / fb - fade) * fb) as usize;
            let o = before.data_off as usize;
            assert_eq!(&bytes[o..o + keep], &original[o..o + keep], "{}", name);
            // fades to 0, and decreases in level
            let v = read_all(&p);
            let last = &v[v.len() - 2..];
            assert!(last.iter().all(|x| x.abs() < 0.01), "{}: {:?}", name, last);
            assert_eq!(analyze_tail(&p).unwrap(), None, "{}", name);
            // the original is kept, and a second fix does not overwrite it
            assert_eq!(fs::read(backup_path(&p)).unwrap(), original);
            fix_tail(&p, 0.0, FADE_MS).unwrap();
            assert_eq!(fs::read(backup_path(&p)).unwrap(), original);
            assert_eq!(WavInfo::read(&p).unwrap().data_len, after.data_len);
        }
    }

    #[test]
    fn fix_without_trim_keeps_the_length_and_odd_pad() {
        let dir = temp_dir("tail_fix_odd");
        // 24 bit mono, odd frame count -> odd data length with pad byte
        let s = sine(RATE as usize + 1, 0.5);
        let p = write(&dir, "odd.wav", wav_bytes(1, 24, 1, &s));
        let len = fs::metadata(&p).unwrap().len();
        fix_tail(&p, 0.0, FADE_MS).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().len(), len);
        assert_eq!(analyze_tail(&p).unwrap(), None);
        let v = read_all(&p);
        assert_eq!(v.len(), RATE as usize + 1);
        assert_eq!(*v.last().unwrap(), 0.0);
    }

    #[test]
    fn own_fade_of_a_full_scale_signal_counts_as_clean() {
        let dir = temp_dir("tail_full");
        // square-ish full-scale bass, cut hard: the worst case for the fade
        let s: Vec<f64> = (0..RATE as usize).map(|i| if (i / 240) % 2 == 0 { 0.98 } else { -0.98 }).collect();
        let p = write(&dir, "loud.wav", wav_bytes(1, 16, 1, &s));
        assert!(analyze_tail(&p).unwrap().is_some());
        fix_tail(&p, 0.0, FADE_MS).unwrap();
        assert_eq!(analyze_tail(&p).unwrap(), None);
    }

    #[test]
    fn fix_refuses_too_short_files() {
        let dir = temp_dir("tail_short");
        let p = write(&dir, "short.wav", wav_bytes(1, 16, 1, &sine(100, 0.5)));
        let original = fs::read(&p).unwrap();
        assert_eq!(fix_tail(&p, 0.0, FADE_MS).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(&p).unwrap(), original);
        assert!(!backup_path(&p).exists());
    }

    #[test]
    fn backup_names() {
        assert_eq!(backup_path(r"C:\a\Song-1-Bass.wav"), Path::new(r"C:\a\Song-1-Bass.orig.wav"));
        assert!(is_backup_wav(r"C:\a\Song-1-Bass.ORIG.WAV"));
        assert!(!is_backup_wav(r"C:\a\Song-1-Bass.wav"));
    }
}

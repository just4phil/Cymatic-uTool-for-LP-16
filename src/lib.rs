//! lp16-tool - writes the song.set files that a Cymatic Audio LP-16 expects.
//!
//! No external crates needed. It reads a uTool2 .songprj file, resolves the
//! {$AudioLib} placeholder, computes the `validationcode` exactly like uTool2
//! does, and writes song.set into an output folder.
//!
//! The command line is in main.rs, the export to a stick in sync.rs.

use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

pub mod backup_mix;
#[cfg(feature = "mp3")]
pub mod backup;
pub mod mix;
pub mod path_key;
pub mod project_lib;
pub mod setlist;
#[cfg(feature = "player")]
pub mod audio_out;
#[cfg(feature = "player")]
pub mod rehearsal;
#[cfg(feature = "player")]
pub mod rehearsal_queue;
/// The rehearsal player's session: queue, stream life cycle, song mix.
#[cfg(feature = "player")]
pub mod rehearsal_session;
#[cfg(feature = "player")]
pub mod rehearsal_midi;
#[cfg(feature = "player")]
pub mod midi_out;
pub mod sync;
pub mod wav_tail;

// ---------------------------------------------------------------------------
// A very small XML reader. A .songprj is simple, flat XML with an "ns0:"
// prefix, so a text search is enough here.
// ---------------------------------------------------------------------------

/// Text of the first <ns0:name> element, with XML entities decoded
/// ("Rock &amp; Roll" -> "Rock & Roll").
pub fn tag(xml: &str, name: &str) -> Option<String> {
    plain_tag(xml, &format!("ns0:{}", name))
}

/// Text of the first <name> element (no namespace, e.g. song.set / .play),
/// with XML entities decoded.
pub fn plain_tag(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{}>", name);
    let close = format!("</{}>", name);
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml_unescape(&xml[start..end]))
}

/// Replace the text of the first <ns0:name> element. `value` is plain
/// text and gets XML-escaped here. Without that element xml is unchanged.
pub fn set_tag(xml: &str, name: &str, value: &str) -> String {
    let open = format!("<ns0:{}>", name);
    let close = format!("</ns0:{}>", name);
    if let Some(s) = xml.find(&open) {
        let start = s + open.len();
        if let Some(e) = xml[start..].find(&close) {
            return format!("{}{}{}", &xml[..start], xml_escape(value), &xml[start + e..]);
        }
    }
    xml.to_string()
}

/// Decode the five predefined XML entities and numeric character
/// references (&#38; / &#x26;). Unknown entities stay as they are.
pub fn xml_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let after = &rest[p..];
        let decoded = after.find(';').filter(|&e| e <= 10).and_then(|e| {
            let ent = &after[1..e];
            let ch = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                    u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
                }
                _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
                _ => None,
            };
            ch.map(|c| (c, e + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &after[len..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn resolve(path: &str, audio_lib: &str) -> String {
    path.replace("{$AudioLib}", audio_lib)
}

fn sr_code(sr: u32) -> i32 {
    match sr {
        44100 => 1,
        48000 => 2,
        88200 => 3,
        96000 => 4,
        _ => 6,
    }
}

fn bit_code(bits: u32) -> i32 {
    match bits {
        16 => 1,
        24 => 2,
        32 => 3,
        _ => 4,
    }
}

pub fn mtime_ms(path: &str) -> i64 {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let modified = match meta.modified() {
        Ok(t) => t,
        Err(_) => return 0,
    };
    match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(_) => 0,
    }
}

pub fn hash_code64(s: &str) -> u64 {
    let mut h: u64 = 0;
    for ch in s.chars() {
        h = h.wrapping_mul(101).wrapping_add(ch as u64);
    }
    h
}

fn put_i32(buf: &mut Vec<u8>, v: i32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_i64(buf: &mut Vec<u8>, v: i64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

// ---------------------------------------------------------------------------
// The validation code: MD5 over a little-endian buffer.
// ---------------------------------------------------------------------------

/// The little-endian buffer the validationcode is the MD5 of.
fn validation_buffer(p: &SongProject) -> Vec<u8> {
    let sync_units: i32 = tag(&p.xml, "sync_units")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    let mut buf = Vec::new();
    put_i32(&mut buf, sr_code(p.sampling_rate));
    put_i32(&mut buf, bit_code(p.resolution));
    // uTool2 uses the same song length for the validation buffer and the playlist.
    buf.extend_from_slice(&p.duration().to_le_bytes());

    let midi_full = resolve(&p.midi_file, &p.audio_lib);
    let midi_hash = if p.midi_file.is_empty() {
        0u64
    } else {
        hash_code64(&midi_full)
    };
    put_i64(&mut buf, midi_hash as i64);
    put_i64(&mut buf, mtime_ms(&midi_full));
    put_i32(&mut buf, sync_units);

    for t in &p.tracks {
        let wav = p.wav(t);
        let file_mtime = mtime_ms(&wav);
        let track_sr = if file_mtime == t.vts { t.sr } else { 6 };
        put_i32(&mut buf, t.channel);
        put_i64(&mut buf, hash_code64(&wav) as i64);
        put_i64(&mut buf, file_mtime);
        put_i32(&mut buf, t.ati);
        put_i32(&mut buf, track_sr);
    }
    buf
}

const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20,
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee,
    0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be,
    0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa,
    0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
    0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c,
    0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05,
    0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039,
    0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1,
    0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

pub fn md5(input: &[u8]) -> [u8; 16] {
    let mut msg = input.to_vec();
    let bit_len = (msg.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0) = (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);

    for chunk in msg.chunks(64) {
        let mut m = [0u32; 16];
        for j in 0..16 {
            m[j] = u32::from_le_bytes([
                chunk[j * 4],
                chunk[j * 4 + 1],
                chunk[j * 4 + 2],
                chunk[j * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = if i < 16 {
                ((b & c) | (!b & d), i)
            } else if i < 32 {
                ((d & b) | (!d & c), (5 * i + 1) % 16)
            } else if i < 48 {
                (b ^ c ^ d, (3 * i + 5) % 16)
            } else {
                (c ^ (b | !d), (7 * i) % 16)
            };
            let tmp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(MD5_K[i])
                    .wrapping_add(m[g])
                    .rotate_left(MD5_S[i]),
            );
            a = tmp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

// ---------------------------------------------------------------------------
// song.set output (UTF-8, CRLF line endings, exactly like uTool2 writes it)
// ---------------------------------------------------------------------------

/// XML-escape a text value: &, <, > (uTool2 only ever writes these in
/// text nodes; quotes matter only in attributes, which we generate
/// ourselves). Stops invalid XML when a song name contains & or <.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// song.set text. The header values are copied as text from the .songprj
/// (not re-formatted), so the file stays byte-equal with uTool2's.
fn generate_song_set(p: &SongProject, validation: &str) -> String {
    let xml = &p.xml;
    let tracks = &p.tracks;
    // uTool2 rounds midi_length commercially (161.89 -> 162), it does not cut.
    let midi_length = p.midi_duration.round() as i64;
    let midi_samples = p.midi_length_samples();
    let song_name = xml_escape(&tag(xml, "song_name").unwrap_or_default());
    let timestamp = xml_escape(&tag(xml, "timestamp").unwrap_or_default());
    let sampling_rate = xml_escape(&tag(xml, "sampling_rate").unwrap_or_default());
    let resolution = xml_escape(&tag(xml, "resolution").unwrap_or_default());
    let midi_name = xml_escape(
        &Path::new(&p.midi_file)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );

    let mut lines: Vec<String> = Vec::new();
    lines.push("<?xml version=\"1.0\" encoding=\"utf-8\"?>".into());
    lines.push(String::new());
    lines.push("<song_settings>".into());
    lines.push("  <version>2.0</version>".into());
    lines.push(format!("  <timestamp>{}</timestamp>", timestamp));
    lines.push("  <creator>cymatic audio uTool</creator>".into());
    lines.push("  <target_device>LP-16</target_device>".into());
    lines.push(format!("  <song_name>{}</song_name>", song_name));
    lines.push(format!("  <validationcode>{}</validationcode>", validation));
    lines.push("  <playback_type>multitrack</playback_type>".into());
    lines.push(format!("  <channel_count>{}</channel_count>", tracks.len()));
    lines.push(format!("  <sampling_rate>{}</sampling_rate>", sampling_rate));
    lines.push(format!("  <resolution>{}</resolution>", resolution));
    // uTool2 leaves the MIDI block out entirely when the song has no MIDI file
    if !midi_name.is_empty() {
        lines.push(format!("  <midi_file>{}</midi_file>", midi_name));
        lines.push(format!("  <midi_length>{}</midi_length>", midi_length));
        lines.push(format!("  <midi_length_samples>{}</midi_length_samples>", midi_samples));
        lines.push("  <midi_offset>0</midi_offset>".into());
    }

    let sections: [(&str, fn(&ProjectTrack) -> String); 3] = [
        ("output_level", |t| xml_escape(&t.monitor_level)),
        ("monitor_pan", |t| {
            let v: f64 = t.monitor_pan.parse().unwrap_or(0.0);
            format!("{}", v as i64)
        }),
        ("channel_mute", |t| xml_escape(&t.monitor_mute)),
    ];
    for (label, pick) in sections.iter() {
        lines.push(format!("  <{}>", label));
        for (i, t) in tracks.iter().enumerate() {
            lines.push(format!("    <chan id=\"{}\">{}</chan>", i + 1, pick(t)));
        }
        lines.push(format!("  </{}>", label));
    }
    lines.push("</song_settings>".into());
    lines.push(String::new());
    lines.join("\r\n")
}

// ---------------------------------------------------------------------------
// Playlists: read a .playlistprj, write a .play (UTF-8, CRLF, like uTool2).
// ---------------------------------------------------------------------------

/// One song entry of a .playlistprj / .play.
#[derive(Clone)]
pub struct PlaylistSong {
    /// Song folder name on the stick, e.g. "_KIDS_(2026-05-10_RF)".
    pub name: String,
    /// Path to the .songprj, with {$ProjectLib} resolved.
    pub songprj: String,
    /// pause mode, e.g. "wait_for_button_press".
    pub mode: String,
    /// pause time in seconds.
    pub delay: String,
}

/// How a playlist entry starts on the LP-16: `start_mode` of the
/// .playlistprj, `pause_mode/mode` of the .play. The values are the ones
/// uTool2 writes; the delay (`start_mode_delay`, `pause_mode/time`) is in
/// seconds and only matters for [`StartMode::Timer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartMode {
    /// "wait_for_button_press": the LP-16 waits for Play
    Button,
    /// "wait_period": starts by itself after the delay
    Timer,
    /// "no_wait": starts by itself right away
    NoWait,
}

impl StartMode {
    pub const ALL: [StartMode; 3] = [StartMode::Button, StartMode::Timer, StartMode::NoWait];

    /// The mode of a file value; an empty value is [`StartMode::Button`]
    /// (like a new song), an unknown one None.
    pub fn parse(value: &str) -> Option<StartMode> {
        match value.trim() {
            "" | "wait_for_button_press" => Some(StartMode::Button),
            "wait_period" => Some(StartMode::Timer),
            "no_wait" => Some(StartMode::NoWait),
            _ => None,
        }
    }

    /// The value written to the .playlistprj and the .play.
    pub fn as_str(self) -> &'static str {
        match self {
            StartMode::Button => "wait_for_button_press",
            StartMode::Timer => "wait_period",
            StartMode::NoWait => "no_wait",
        }
    }

    /// The name uTool2 shows.
    pub fn label(self) -> &'static str {
        match self {
            StartMode::Button => "Wait for button press",
            StartMode::Timer => "Wait for timer",
            StartMode::NoWait => "No wait",
        }
    }
}

/// Whole seconds of a `start_mode_delay` text (empty or invalid gives 0).
pub fn delay_seconds(text: &str) -> u32 {
    text.trim().replace(',', ".").parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0).map(|v| v.round().min(u32::MAX as f64) as u32).unwrap_or(0)
}

/// Text for a start mode and its delay, e.g. "Wait for timer (5 s)";
/// an unknown mode is shown as it is.
pub fn start_mode_text(mode: &str, delay: &str) -> String {
    match StartMode::parse(mode) {
        Some(StartMode::Timer) => format!("Wait for timer ({} s)", delay_seconds(delay)),
        Some(m) => m.label().to_string(),
        None => format!("Unknown start mode \"{}\"", mode),
    }
}

/// Song name, pause mode, pause delay and the .songprj path for each
/// <ns0:song_project> of a .playlistprj. Durations are NOT read here;
/// they come from SongProject::duration() of each .songprj.
/// A song location of a .playlistprj: `{$ProjectLib}\...` below
/// `project_lib`, with one `\` between them (also for "D:\").
fn resolve_project_lib(location: &str, project_lib: &str) -> String {
    match location.strip_prefix("{$ProjectLib}") {
        Some(rest) => path_key::join_root(project_lib, rest),
        None => location.to_string(),
    }
}

pub fn parse_playlist(xml: &str, project_lib: &str) -> (String, Vec<PlaylistSong>) {
    let name = tag(xml, "playlist_name").unwrap_or_default();
    let mut songs = Vec::new();
    let marker = "<ns0:song_project ";
    let mut rest = xml;
    while let Some(pos) = rest.find(marker) {
        let after_open = &rest[pos..];
        let close = "</ns0:song_project>";
        let end = match after_open.find(close) {
            Some(e) => e,
            None => break,
        };
        let block = &after_open[..end];
        rest = &after_open[end + close.len()..];
        songs.push(PlaylistSong {
            name: tag(block, "song_project_name").unwrap_or_default(),
            songprj: resolve_project_lib(&tag(block, "project_file_location").unwrap_or_default(), project_lib),
            mode: tag(block, "start_mode").unwrap_or_default(),
            delay: tag(block, "start_mode_delay").unwrap_or_default(),
        });
    }
    (name, songs)
}

/// The .play file for the stick. Same layout as uTool2: XML header, blank
/// line, one <song> block per entry, three decimals for the duration.
pub fn generate_play(
    playlist_name: &str,
    timestamp_ms: i64,
    songs: &[PlaylistSong],
    duration_of: impl Fn(&PlaylistSong) -> f64,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push("<?xml version=\"1.0\" encoding=\"utf-8\"?>".into());
    lines.push(String::new());
    lines.push("<playlist>".into());
    lines.push("  <version>1.0</version>".into());
    lines.push(format!("  <timestamp>{}</timestamp>", timestamp_ms));
    lines.push("  <creator>cymatic audio uTool</creator>".into());
    lines.push(format!("  <playlistname>{}</playlistname>", xml_escape(playlist_name)));
    lines.push("  <target_device>LP-16</target_device>".into());
    for song in songs {
        lines.push("  <song>".into());
        lines.push(format!("    <name>{}</name>", xml_escape(&song.name)));
        lines.push("    <location></location>".into());
        lines.push(format!(
            "    <duration>{:.3}</duration>",
            duration_of(song)
        ));
        lines.push("    <pause_mode>".into());
        lines.push(format!("      <mode>{}</mode>", xml_escape(&song.mode)));
        lines.push(format!("      <time>{}</time>", xml_escape(&song.delay)));
        lines.push("    </pause_mode>".into());
        lines.push("  </song>".into());
    }
    lines.push("</playlist>".into());
    lines.push(String::new());
    lines.join("\r\n")
}

// ---------------------------------------------------------------------------
// WAV export: rewrite the header exactly like uTool2, copy the PCM payload.
// The stick files are NOT gain-processed: their data payload is byte-identical
// to the source; uTool2 only rebuilds the header (JUNK + 16-byte fmt + data)
// so that the device always sees the same layout.
// ---------------------------------------------------------------------------

/// Bytes in front of the payload of a WAV exported to the stick:
/// RIFF header + JUNK (52) + fmt (16) + data chunk header.
pub const STICK_WAV_HEADER_LEN: u64 = 104;

/// Kind of the samples in a WAV payload, from the "fmt " chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    /// integer PCM (format tag 1)
    Int,
    /// IEEE float (format tag 3)
    Float,
    /// anything else, including a WAVE_FORMAT_EXTENSIBLE chunk that is too
    /// short to name its sub-format
    Unknown,
}

/// Format and payload position of a WAV file. The one reader for every
/// WAV: export, stale-file check, import and song editor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WavInfo {
    pub channels: u16,
    pub rate: u32,
    pub bits: u16,
    pub format: SampleFormat,
    /// file offset of the PCM payload
    pub data_off: u64,
    /// length of the PCM payload in bytes
    pub data_len: u64,
}

impl WavInfo {
    /// Read the chunk headers only (the audio is never read).
    /// Required: "RIFF"…"WAVE", a "fmt " chunk (at least 16 bytes, channels,
    /// rate and bits not 0) and a "data" chunk that lies completely inside
    /// the file, in either order. Other chunks are skipped (padded to even
    /// sizes). Anything else is `InvalidData`.
    pub fn read(src: &str) -> std::io::Result<WavInfo> {
        use std::io::{Read, Seek, SeekFrom};
        let bad = || {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not a usable WAV file", src),
            )
        };
        let mut f = fs::File::open(src)?;
        let file_len = f.metadata()?.len();
        let mut head = [0u8; 12];
        f.read_exact(&mut head).map_err(|_| bad())?;
        if &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
            return Err(bad());
        }
        let mut fmt: Option<(u16, u32, u16, SampleFormat)> = None;
        let mut data: Option<(u64, u64)> = None;
        let mut pos = 12u64;
        while pos + 8 <= file_len {
            f.seek(SeekFrom::Start(pos))?;
            let mut ch = [0u8; 8];
            f.read_exact(&mut ch)?;
            let size = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]) as u64;
            let body = pos + 8;
            match &ch[0..4] {
                b"fmt " if size >= 16 => {
                    let mut b = [0u8; 16];
                    f.read_exact(&mut b).map_err(|_| bad())?;
                    let mut tag = u16::from_le_bytes([b[0], b[1]]);
                    if tag == 0xFFFE && size >= 40 {
                        // WAVE_FORMAT_EXTENSIBLE: the sub-format GUID starts
                        // at offset 24 of the chunk body; its first two
                        // bytes are the real format tag
                        let mut ext = [0u8; 10];
                        f.read_exact(&mut ext).map_err(|_| bad())?;
                        tag = u16::from_le_bytes([ext[8], ext[9]]);
                    }
                    let format = match tag {
                        1 => SampleFormat::Int,
                        3 => SampleFormat::Float,
                        _ => SampleFormat::Unknown,
                    };
                    fmt = Some((
                        u16::from_le_bytes([b[2], b[3]]),
                        u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
                        u16::from_le_bytes([b[14], b[15]]),
                        format,
                    ));
                }
                b"data" => {
                    if body + size > file_len {
                        return Err(bad());
                    }
                    data = Some((body, size));
                }
                _ => {}
            }
            if let (Some((channels, rate, bits, format)), Some((data_off, data_len))) = (fmt, data) {
                if channels == 0 || rate == 0 || bits == 0 {
                    return Err(bad());
                }
                return Ok(WavInfo { channels, rate, bits, format, data_off, data_len });
            }
            pos = body + size + (size % 2); // chunks are padded to even sizes
        }
        Err(bad())
    }

    /// length in seconds
    pub fn seconds(&self) -> f64 {
        let frame = self.channels as u64 * (self.bits as u64 / 8);
        if frame == 0 {
            return 0.0;
        }
        (self.data_len / frame) as f64 / self.rate as f64
    }

    /// Size of this WAV after export to the stick: header + payload + pad
    /// byte for odd payload lengths. Used to detect stale stick files.
    pub fn stick_size(&self) -> u64 {
        STICK_WAV_HEADER_LEN + self.data_len + self.data_len % 2
    }
}

/// Bytes read from disk per `WavReader` refill.
const WAV_READ_CHUNK: usize = 64 * 1024;

/// Reads the audio of a WAV file as interleaved `f32` frames from any frame
/// position. Built on `WavInfo::read` (no second WAV parser). Supports
/// 8 bit (unsigned), 16, 24 and 32 bit integer and 32 bit float, any number
/// of channels. Never reads past the end of the "data" chunk.
pub struct WavReader {
    file: fs::File,
    info: WavInfo,
    /// bytes per frame
    frame_bytes: usize,
    /// total frames in the data chunk
    frames: u64,
    /// next frame to be read
    pos: u64,
    scratch: Vec<u8>,
}

impl WavReader {
    /// Open `path`. The error names the file when it cannot be read or the
    /// format is not supported ("unsupported WAV format").
    pub fn open(path: &str) -> std::io::Result<WavReader> {
        let named = |e: std::io::Error| std::io::Error::new(e.kind(), format!("{}: {}", path, e));
        let info = WavInfo::read(path).map_err(named)?;
        let supported = matches!(
            (info.format, info.bits),
            (SampleFormat::Int, 8 | 16 | 24 | 32) | (SampleFormat::Float, 32)
        );
        if !supported {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: unsupported WAV format", path),
            ));
        }
        let file = fs::File::open(path).map_err(named)?;
        let frame_bytes = info.channels as usize * (info.bits as usize / 8);
        let frames = info.data_len / frame_bytes as u64;
        let per_chunk = (WAV_READ_CHUNK / frame_bytes).max(1);
        let mut r = WavReader { file, info, frame_bytes, frames, pos: 0, scratch: vec![0; per_chunk * frame_bytes] };
        r.seek(0).map_err(named)?;
        Ok(r)
    }

    pub fn info(&self) -> &WavInfo {
        &self.info
    }

    pub fn channels(&self) -> u16 {
        self.info.channels
    }

    pub fn rate(&self) -> u32 {
        self.info.rate
    }

    /// total number of frames in the file
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// next frame `read` will deliver
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// Jump to `frame`. Positions after the end are clamped to the end, so
    /// the next `read` returns 0.
    pub fn seek(&mut self, frame: u64) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom};
        let frame = frame.min(self.frames);
        self.file.seek(SeekFrom::Start(self.info.data_off + frame * self.frame_bytes as u64))?;
        self.pos = frame;
        Ok(())
    }

    /// Fill `out` with interleaved samples from the current position and
    /// return the number of FRAMES read (`out.len() / channels` at most).
    /// Fewer than requested means the end of the data; afterwards it
    /// returns 0. A disk error ends the data like the end of the file.
    pub fn read(&mut self, out: &mut [f32]) -> usize {
        use std::io::Read;
        let ch = self.info.channels as usize;
        let want = ((out.len() / ch) as u64).min(self.frames - self.pos) as usize;
        let sample_bytes = self.info.bits as usize / 8;
        let mut done = 0;
        while done < want {
            let n = (want - done).min(self.scratch.len() / self.frame_bytes);
            let bytes = &mut self.scratch[..n * self.frame_bytes];
            if self.file.read_exact(bytes).is_err() {
                self.pos = self.frames; // unreadable rest: end of data
                break;
            }
            let dst = &mut out[done * ch..(done + n) * ch];
            for (o, b) in dst.iter_mut().zip(bytes.chunks_exact(sample_bytes)) {
                *o = match (self.info.format, sample_bytes) {
                    (SampleFormat::Float, _) => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                    (_, 1) => (b[0] as f32 - 128.0) / 128.0,
                    (_, 2) => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
                    (_, 3) => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8388608.0,
                    _ => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2147483648.0,
                };
            }
            done += n;
            self.pos += n as u64;
        }
        done
    }
}

/// The playable tracks of a song, as the rehearsal player and the MP3
/// backup use them.
pub struct SongTracks {
    /// sample rate of the song: that of the first readable track, or the
    /// .songprj value without any
    pub rate: u32,
    /// (0-based LP-16 output, reader) of every usable track
    pub tracks: Vec<(usize, WavReader)>,
    /// why tracks were left out
    pub warnings: Vec<String>,
}

/// Open the WAVs of `project`. Tracks that are missing, unreadable,
/// unsupported, not mono, not on output 1-16 or of another sample rate
/// than the first readable track are left out with a warning.
pub fn open_song_tracks(project: &SongProject) -> SongTracks {
    const OUTPUTS: usize = mix::STRIPS;
    let mut warnings = Vec::new();
    let mut readers: Vec<(String, usize, WavReader)> = Vec::new();
    for (i, t) in project.tracks.iter().enumerate() {
        let name = if t.name.is_empty() { format!("channel {}", i + 1) } else { t.name.clone() };
        if t.channel < 0 || t.channel as usize >= OUTPUTS {
            warnings.push(format!(
                "Track \"{}\": output channel {} is not in 1-{}, left silent",
                name,
                t.channel as i64 + 1,
                OUTPUTS
            ));
            continue;
        }
        match WavReader::open(&project.wav(t)) {
            Ok(r) => readers.push((name, t.channel as usize, r)),
            Err(e) => warnings.push(format!("Track \"{}\": {}", name, e)),
        }
    }
    let rate = readers.first().map(|(_, _, r)| r.rate()).unwrap_or(project.sampling_rate);
    let mut tracks = Vec::new();
    for (name, channel, r) in readers {
        if r.channels() != 1 {
            warnings.push(format!("Track \"{}\": not mono ({} channels), left silent", name, r.channels()));
        } else if r.rate() != rate {
            warnings.push(format!(
                "Track \"{}\": sample rate {} Hz differs from the song rate {} Hz, left silent",
                name,
                r.rate(),
                rate
            ));
        } else {
            tracks.push((channel, r));
        }
    }
    SongTracks { rate, tracks, warnings }
}

// ---------------------------------------------------------------------------
// Safe writing to the stick.
// Every file is first written to "<name>.lp16tmp", flushed to the device
// and only then renamed to its final name. If the export is interrupted
// (stick pulled, power loss, Cancel), the stick keeps the old file or a
// leftover .lp16tmp, never a half-written song.set / WAV / .play.
// ---------------------------------------------------------------------------

/// Suffix of temporary files written during an export.
pub const TMP_SUFFIX: &str = ".lp16tmp";

fn tmp_path(dest: &Path) -> std::path::PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(TMP_SUFFIX);
    std::path::PathBuf::from(name)
}

/// Write `dest` atomically: fill a temporary file via `fill`, flush it to
/// the device, then rename it over `dest` (replaces an existing file).
pub fn write_atomic<F>(dest: &Path, fill: F) -> std::io::Result<()>
where
    F: FnOnce(&mut std::io::BufWriter<fs::File>) -> std::io::Result<()>,
{
    use std::io::Write;
    let tmp = tmp_path(dest);
    let result = (|| {
        let file = fs::File::create(&tmp)?;
        let mut w = std::io::BufWriter::with_capacity(1 << 20, file);
        fill(&mut w)?;
        w.flush()?;
        let file = w.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, dest)?;
        // replacing "abc.wav" by "ABC.wav" keeps the old spelling on FAT/NTFS
        fix_name_case(dest).map(|_| ())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// The entry of `path`'s folder that is `path` apart from upper / lower
/// case, if its actual name is spelled differently.
pub fn name_case_differs(path: &Path) -> Option<String> {
    let want = path.file_name()?.to_string_lossy().into_owned();
    let key = want.to_lowercase();
    fs::read_dir(path.parent()?)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n != &want && n.to_lowercase() == key)
}

/// Windows and FAT ignore upper / lower case, the LP-16 firmware may not:
/// a file or folder that exists as "abc" while `path` says "ABC" is renamed
/// to the exact spelling (via a temporary name). Ok(true): it was renamed.
pub fn fix_name_case(path: &Path) -> std::io::Result<bool> {
    let Some(actual) = name_case_differs(path) else { return Ok(false) };
    let dir = path.parent().unwrap_or(Path::new(""));
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".lp16case");
    fs::rename(dir.join(&actual), &tmp)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::rename(&tmp, dir.join(&actual));
        return Err(e);
    }
    Ok(true)
}

/// Write bytes atomically (song.set, .play).
pub fn write_bytes_atomic(dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    write_atomic(dest, |w| w.write_all(bytes))
}

/// Copy a file atomically (MIDI).
pub fn copy_atomic(src: &str, dest: &Path) -> std::io::Result<()> {
    let mut input = fs::File::open(src)?;
    write_atomic(dest, |w| std::io::copy(&mut input, w).map(|_| ()))
}

/// Remove leftover temporary files of an interrupted export in `dir`.
pub fn remove_tmp_files(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().ends_with(TMP_SUFFIX) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

/// Rebuild the WAV like uTool2 does on export:
/// "RIFF" size "WAVE" + "JUNK " 52 zero bytes + "fmt " 16 + "data" payload,
/// plus one pad byte if the payload length is odd.
/// The payload is streamed (never held in memory as a whole) and the file
/// is written atomically.
pub fn export_wav(src: &str, dest: &str) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let WavInfo { channels, rate, bits, data_off, data_len, .. } = WavInfo::read(src)?;
    let pad = (data_len % 2) as u32; // pad byte for odd payload lengths
    let block_align = channels * bits / 8;
    let byte_rate = rate * block_align as u32;
    let riff_size = 4u32 + (8 + 52) as u32 + (8 + 16) as u32 + (8 + data_len as u32 + pad);

    let mut header: Vec<u8> = Vec::with_capacity(STICK_WAV_HEADER_LEN as usize);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&riff_size.to_le_bytes());
    header.extend_from_slice(b"WAVE");
    header.extend_from_slice(b"JUNK");
    header.extend_from_slice(&52u32.to_le_bytes());
    header.extend_from_slice(&[0u8; 52]);
    header.extend_from_slice(b"fmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&(1u16).to_le_bytes()); // PCM
    header.extend_from_slice(&channels.to_le_bytes());
    header.extend_from_slice(&rate.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&block_align.to_le_bytes());
    header.extend_from_slice(&bits.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&(data_len as u32).to_le_bytes());

    let mut input = fs::File::open(src)?;
    input.seek(SeekFrom::Start(data_off))?;
    write_atomic(Path::new(dest), |w| {
        w.write_all(&header)?;
        let copied = std::io::copy(&mut (&mut input).take(data_len), w)?;
        if copied != data_len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("{}: data chunk shorter than expected", src),
            ));
        }
        if pad == 1 {
            w.write_all(&[0])?;
        }
        Ok(())
    })
}

/// Destination file name of a track WAV on the stick:
/// "_" + source stem + "_" + two-digit channel (1-based), like uTool2,
/// e.g. "â€¦\2026-05-10_RF-1-Klick-7.wav" -> "_2026-05-10_RF-1-Klick-7_01.wav".
pub fn stick_wav_name(src: &str, channel: i32) -> String {
    let stem = Path::new(src)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("_{}_{:02}.wav", stem, channel + 1)
}

pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Sync plan and export: shared by the CLI and the GUI.
// ---------------------------------------------------------------------------

/// Song file set on the stick: WAVs (renamed by channel), MIDI, song.set.
pub struct SongPlan {
    pub name: String,
    pub needs_export: bool,
    pub reason: String,
    pub text: String,
    /// (src, dest) with uTool2 naming
    pub wav: Vec<(String, String)>,
    /// (src, dest) of the MIDI file, if the song has one
    pub midi: Option<(String, String)>,
    /// bytes a full export writes: every WAV, the MIDI file, song.set
    pub bytes_full: u64,
    /// bytes uTool2 shows as "To Copy" for a Quick Export: missing or
    /// changed WAVs, a missing MIDI file, song.set
    pub bytes_changed: u64,
}

/// Decide whether a song on the stick needs an export, and prepare the
/// full file set (song.set text, wav/midi destinations).
///
/// The cheap checks (folder, WAV present and of the right size, MIDI
/// present) come first. A MIDI file that was changed on the source is NOT
/// caught by "midi file missing": its mtime is part of the validationcode,
/// so it shows up as "song.set changed". The same goes for a WAV that was
/// rewritten with the same length.
pub fn plan_song(project: &SongProject, songs_root: &str) -> SongPlan {
    let text = project.song_set();
    let name = project.name.clone();

    let dir = Path::new(songs_root).join(&name);
    let mut wav = Vec::new();
    let mut missing_wav = 0usize;
    let mut stale_wav = 0usize;
    let mut unreadable_source = 0usize;
    let mut bytes_full = text.len() as u64;
    let mut bytes_changed = text.len() as u64;
    for t in &project.tracks {
        let src = project.wav(t);
        let dest = dir.join(stick_wav_name(&src, t.channel));
        // A source WAV that cannot be read would silently corrupt the
        // validation code (mtime 0); count it instead.
        match WavInfo::read(&src).map(|i| i.stick_size()) {
            Err(_) => unreadable_source += 1,
            Ok(expected) => {
                bytes_full += expected;
                match fs::metadata(&dest) {
                    Err(_) => {
                        missing_wav += 1;
                        bytes_changed += expected;
                    }
                    Ok(meta) => {
                        if meta.len() != expected {
                            stale_wav += 1;
                            bytes_changed += expected;
                        }
                    }
                }
            }
        }
        wav.push((src, dest.to_string_lossy().into_owned()));
    }
    let midi = project.midi_path().map(|midi_full| {
        let dest = dir.join(
            Path::new(&midi_full)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        );
        (midi_full, dest.to_string_lossy().into_owned())
    });

    let song_set = dir.join("song.set");
    let midi_missing = midi
        .as_ref()
        .map(|(_, dest)| !Path::new(dest).exists())
        .unwrap_or(false);
    if let Some((src, _)) = &midi {
        let size = fs::metadata(src).map(|m| m.len()).unwrap_or(0);
        bytes_full += size;
        if midi_missing {
            bytes_changed += size;
        }
    }
    // files that exist on the stick, but spelled differently (see fix_name_case)
    let case_wrong = wav
        .iter()
        .map(|(_, d)| d.as_str())
        .chain(midi.as_ref().map(|(_, d)| d.as_str()))
        .map(Path::new)
        .chain(std::iter::once(song_set.as_path()))
        .filter(|p| name_case_differs(p).is_some())
        .count();
    let (needs_export, reason) = if !Path::new(&songs_root).join(&name).exists() {
        (true, "new song".to_string())
    } else if unreadable_source > 0 {
        (
            true,
            format!("{} source wav file(s) unreadable", unreadable_source),
        )
    } else if missing_wav > 0 {
        (true, format!("{} wav file(s) missing", missing_wav))
    } else if stale_wav > 0 {
        (true, format!("{} wav file(s) changed on source", stale_wav))
    } else if midi_missing {
        (true, "midi file missing".to_string())
    } else if case_wrong > 0 {
        (true, format!("{} file name(s) spelled differently on the stick", case_wrong))
    } else {
        match fs::read_to_string(&song_set) {
            Ok(existing) => {
                if existing == text {
                    (false, "up to date".to_string())
                } else {
                    (true, "song.set changed".to_string())
                }
            }
            Err(_) => (true, "song.set missing".to_string()),
        }
    };

    SongPlan {
        name,
        needs_export,
        reason,
        text,
        wav,
        midi,
        bytes_full,
        bytes_changed,
    }
}

/// Run one prepared song export: WAVs, MIDI, song.set (each written
/// atomically, song.set last), then check the result on the target.
pub fn export_plan(plan: &SongPlan, songs_root: &str) -> std::io::Result<()> {
    let dir = Path::new(songs_root).join(&plan.name);
    fs::create_dir_all(&dir)?;
    fix_name_case(&dir)?;
    remove_tmp_files(&dir);
    for (src, dest) in &plan.wav {
        export_wav(src, dest)?;
    }
    if let Some((src, dest)) = &plan.midi {
        copy_atomic(src, Path::new(dest))?;
    }
    write_bytes_atomic(&dir.join("song.set"), plan.text.as_bytes())?;
    verify_plan(plan, songs_root).map_err(|msg| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("verification failed: {}", msg),
        )
    })
}

/// Check a song on the target after the export: song.set byte-equal,
/// every WAV with the expected size, MIDI with the size of its source.
pub fn verify_plan(plan: &SongPlan, songs_root: &str) -> Result<(), String> {
    let dir = Path::new(songs_root).join(&plan.name);
    let set_path = dir.join("song.set");
    match fs::read(&set_path) {
        Ok(bytes) if bytes == plan.text.as_bytes() => {}
        Ok(_) => return Err(format!("{} differs", set_path.display())),
        Err(e) => return Err(format!("{}: {}", set_path.display(), e)),
    }
    for (src, dest) in &plan.wav {
        let expected = WavInfo::read(src).map(|i| i.stick_size()).map_err(|e| e.to_string())?;
        match fs::metadata(dest) {
            Ok(m) if m.len() == expected => {}
            Ok(m) => {
                return Err(format!(
                    "{} has {} bytes, expected {}",
                    dest,
                    m.len(),
                    expected
                ))
            }
            Err(e) => return Err(format!("{}: {}", dest, e)),
        }
    }
    if let Some((src, dest)) = &plan.midi {
        let want = fs::metadata(src)
            .map(|m| m.len())
            .map_err(|e| format!("{}: {}", src, e))?;
        match fs::metadata(dest) {
            Ok(m) if m.len() == want => {}
            Ok(_) => return Err(format!("{} differs in size from its source", dest)),
            Err(e) => return Err(format!("{}: {}", dest, e)),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// .playlistprj writing (for playlists created or edited in the GUI).
// ---------------------------------------------------------------------------

/// Write a .playlistprj exactly like uTool2 does, with per-song start
/// mode and delay (PlaylistEditor, fnd_17). `songs` are resolved
/// PlaylistSongs (songprj = full path); the {$ProjectLib} placeholder is
/// re-inserted from project_lib. The song project name is always the
/// .songprj file stem.
pub fn generate_playlistprj(
    playlist_name: &str,
    timestamp_ms: i64,
    project_lib: &str,
    songs: &[PlaylistSong],
) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push("<?xml version=\"1.0\" encoding=\"utf-8\"?>".into());
    lines.push(String::new());
    lines.push(
        "<ns0:playlist_project xmlns:ns0=\"http://www.cymaticaudio.com/PlaylistProject/1.0\">"
            .into(),
    );
    lines.push("  <ns0:version>1</ns0:version>".into());
    lines.push(format!("  <ns0:timestamp>{}</ns0:timestamp>", timestamp_ms));
    lines.push(format!(
        "  <ns0:playlist_name>{}</ns0:playlist_name>",
        xml_escape(playlist_name)
    ));
    lines.push("  <ns0:project_type>multitrackplaylist</ns0:project_type>".into());
    lines.push("  <ns0:song_projects>".into());
    for (i, s) in songs.iter().enumerate() {
        let rel = path_key::relative_to(&s.songprj, project_lib).unwrap_or(&s.songprj);
        let name = Path::new(&s.songprj)
            .file_stem()
            .map(|x| x.to_string_lossy().into_owned())
            .unwrap_or_default();
        lines.push(format!("    <ns0:song_project id=\"{}\">", i));
        lines.push(format!(
            "      <ns0:project_file_location>{{$ProjectLib}}\\{}</ns0:project_file_location>",
            xml_escape(rel)
        ));
        lines.push(format!(
            "      <ns0:song_project_name>{}</ns0:song_project_name>",
            xml_escape(&name)
        ));
        lines.push(format!(
            "      <ns0:start_mode>{}</ns0:start_mode>",
            xml_escape(&s.mode)
        ));
        lines.push(format!(
            "      <ns0:start_mode_delay>{}</ns0:start_mode_delay>",
            xml_escape(&s.delay)
        ));
        lines.push("    </ns0:song_project>".into());
    }
    lines.push("  </ns0:song_projects>".into());
    lines.push("</ns0:playlist_project>".into());
    lines.push(String::new());
    lines.join("\r\n")
}

/// Write a new, empty .songprj exactly in uTool2's format (fnd_48/evidence
/// ev_91386câ€¦): header fields, empty tracks and markers. The defaults
/// match the LP-16 (48 kHz, 24 bit, sync_units 1).
pub fn generate_songprj(
    song_name: &str,
    timestamp_ms: i64,
    sampling_rate: u32,
    resolution: u32,
    sync_units: i32,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push("<?xml version=\"1.0\" encoding=\"utf-8\"?>".into());
    lines.push(String::new());
    lines.push("<ns0:SongProject xmlns:ns0=\"http://www.cymaticaudio.com/SongProject/1.0\">".into());
    lines.push("  <ns0:version>1</ns0:version>".into());
    lines.push(format!("  <ns0:timestamp>{}</ns0:timestamp>", timestamp_ms));
    lines.push(format!(
        "  <ns0:song_name>{}</ns0:song_name>",
        xml_escape(song_name)
    ));
    lines.push("  <ns0:song_type>multitrack</ns0:song_type>".into());
    lines.push(format!("  <ns0:sync_units>{}</ns0:sync_units>", sync_units));
    lines.push(format!("  <ns0:sampling_rate>{}</ns0:sampling_rate>", sampling_rate));
    lines.push(format!("  <ns0:resolution>{}</ns0:resolution>", resolution));
    lines.push("  <ns0:duration_seconds>0.000</ns0:duration_seconds>".into());
    lines.push("  <ns0:track_count>0</ns0:track_count>".into());
    lines.push("  <ns0:midi_file></ns0:midi_file>".into());
    lines.push("  <ns0:midi_duration>0.000</ns0:midi_duration>".into());
    lines.push("  <ns0:tracks/>".into());
    lines.push("  <ns0:markers/>".into());
    lines.push("</ns0:SongProject>".into());
    lines.push(String::new());
    lines.join("\r\n")
}

// ---------------------------------------------------------------------------
// Song editor: WAV/MIDI facts and rewriting the tracks of a .songprj.
// ---------------------------------------------------------------------------

/// One sendable event of a Standard MIDI File.
#[derive(Debug, Clone, PartialEq)]
pub struct MidiEvent {
    /// Absolute time in seconds from the start of the file.
    pub time: f64,
    /// The complete MIDI message. Channel messages carry their own status
    /// byte (running status is resolved); SysEx `F0` is `F0` + data (incl. the
    /// final `F7` if the file has it); an `F7` escape is its raw data only.
    pub bytes: Vec<u8>,
}

/// Result of reading a Standard MIDI File: the events of all tracks in
/// playing order, the total length and the channels that play notes.
#[derive(Debug, Clone, PartialEq)]
pub struct MidiEvents {
    /// Channel voice messages and SysEx, no meta events. Sorted by time;
    /// at equal time the track order and then the file order decide.
    pub events: Vec<MidiEvent>,
    /// Length in seconds, the value `song.set` gets as `midi_length`.
    pub duration: f64,
    /// MIDI channels (0-based) that have a Note On with velocity > 0.
    pub note_on_channels: [bool; 16],
}

fn smf_varlen(b: &[u8], p: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for _ in 0..4 {
        let c = *b.get(*p)?;
        *p += 1;
        v = (v << 7) | (c & 0x7f) as u64;
        if c & 0x80 == 0 {
            return Some(v);
        }
    }
    Some(v)
}

/// Read a Standard MIDI File (format 0 or 1). `None` if it is unreadable or
/// broken.
pub fn read_midi_events(src: &str) -> Option<MidiEvents> {
    parse_midi_events(&fs::read(src).ok()?)
}

/// Like [`read_midi_events`] on the bytes of a file.
pub fn parse_midi_events(b: &[u8]) -> Option<MidiEvents> {
    if b.len() < 14 || &b[0..4] != b"MThd" {
        return None;
    }
    let be32 = |p: usize| -> Option<u32> {
        b.get(p..p + 4).map(|x| u32::from_be_bytes([x[0], x[1], x[2], x[3]]))
    };
    let hlen = be32(4)? as usize;
    let ntrks = u16::from_be_bytes([b[10], b[11]]) as usize;
    let division = u16::from_be_bytes([b[12], b[13]]);
    let mut tempos: Vec<(u64, u32)> = Vec::new();
    let mut raw: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut note_on_channels = [false; 16];
    let mut end_tick = 0u64;
    let mut pos = 8 + hlen;
    for _ in 0..ntrks {
        if b.get(pos..pos + 4)? != b"MTrk" {
            return None;
        }
        let len = be32(pos + 4)? as usize;
        let mut p = pos + 8;
        let end = (p + len).min(b.len());
        let mut tick = 0u64;
        let mut running = 0u8;
        while p < end {
            tick += smf_varlen(b, &mut p)?;
            let mut status = *b.get(p)?;
            if status & 0x80 != 0 {
                p += 1;
            } else {
                status = running;
            }
            match status {
                0xff => {
                    let kind = *b.get(p)?;
                    p += 1;
                    let l = smf_varlen(b, &mut p)? as usize;
                    if kind == 0x51 && l == 3 {
                        let t = b.get(p..p + 3)?;
                        tempos.push((tick, u32::from_be_bytes([0, t[0], t[1], t[2]])));
                    }
                    p += l;
                    if kind == 0x2f {
                        break;
                    }
                }
                0xf0 | 0xf7 => {
                    let l = smf_varlen(b, &mut p)? as usize;
                    if p + l <= end {
                        let mut msg = Vec::with_capacity(l + 1);
                        if status == 0xf0 {
                            msg.push(0xf0);
                        }
                        msg.extend_from_slice(&b[p..p + l]);
                        raw.push((tick, msg));
                    }
                    p += l;
                }
                _ => {
                    running = status;
                    let n = match status & 0xf0 {
                        0xc0 | 0xd0 => 1,
                        _ => 2,
                    };
                    // Only well-formed channel messages are sendable; broken
                    // ones (no status, data byte >= 0x80, cut off) still count
                    // for the length but are not sent.
                    if (0x80..0xf0).contains(&status)
                        && p + n <= end
                        && b[p..p + n].iter().all(|d| d & 0x80 == 0)
                    {
                        if status & 0xf0 == 0x90 && b[p + 1] > 0 {
                            note_on_channels[(status & 0x0f) as usize] = true;
                        }
                        let mut msg = vec![status];
                        msg.extend_from_slice(&b[p..p + n]);
                        raw.push((tick, msg));
                    }
                    p += n;
                }
            }
            end_tick = end_tick.max(tick);
        }
        pos += 8 + len;
    }

    // tick -> seconds
    let to_secs: Box<dyn Fn(u64) -> f64> = if division & 0x8000 != 0 {
        // SMPTE time: frames per second * ticks per frame
        let fps = -((division >> 8) as i8) as f64;
        let tpf = (division & 0xff) as f64;
        Box::new(move |t| t as f64 / (fps * tpf))
    } else {
        // Tempo map of all tracks, sorted by tick (stable), 120 BPM at the
        // start; tempos from the end tick on are ignored. Each entry is
        // (tick, seconds at that tick, tempo from there).
        let ppq = division.max(1) as f64;
        tempos.sort_by_key(|t| t.0);
        let mut map: Vec<(u64, f64, u32)> = vec![(0, 0.0, 500_000)];
        let (mut sec, mut last_tick, mut tempo) = (0.0f64, 0u64, 500_000u32);
        for (t, us) in tempos {
            if t >= end_tick {
                break;
            }
            sec += (t - last_tick) as f64 * tempo as f64 / 1e6 / ppq;
            last_tick = t;
            tempo = us;
            map.push((t, sec, us));
        }
        Box::new(move |t| {
            let (mt, msec, mtempo) = map[map.partition_point(|m| m.0 <= t) - 1];
            msec + (t - mt) as f64 * mtempo as f64 / 1e6 / ppq
        })
    };
    raw.sort_by_key(|e| e.0); // stable: track order, then file order
    let events = raw.into_iter().map(|(t, bytes)| MidiEvent { time: to_secs(t), bytes }).collect();
    Some(MidiEvents { events, duration: to_secs(end_tick), note_on_channels })
}

/// Length of a Standard MIDI File in seconds: the last event of all tracks,
/// converted with the tempo map (default 120 BPM).
pub fn midi_duration(src: &str) -> Option<f64> {
    read_midi_events(src).map(|m| m.duration)
}

/// Inverse of resolve(): put the {$AudioLib} placeholder back in front of a
/// path inside the audio library (case-insensitive, like Windows paths).
pub fn unresolve(path: &str, audio_lib: &str) -> String {
    let lib = audio_lib.trim_end_matches('\\');
    if !lib.is_empty()
        && path.len() > lib.len()
        && path[..lib.len()].eq_ignore_ascii_case(lib)
        && path[lib.len()..].starts_with('\\')
    {
        format!("{{$AudioLib}}{}", &path[lib.len()..])
    } else {
        path.to_string()
    }
}

/// One track of a .songprj as written by the song editor.
#[derive(Clone, Debug)]
pub struct ProjectTrack {
    /// 0-based output (slot 1 = 0)
    pub channel: i32,
    /// path as stored, normally with {$AudioLib}
    pub audiofile: String,
    pub vts: i64,
    /// uTool2 sampling-rate code (1 = 44.1k, 2 = 48k, ..., 6 = unknown)
    pub sr: i32,
    pub ati: i32,
    pub name: String,
    pub monitor_level: String,
    pub monitor_pan: String,
    pub monitor_mute: String,
}

impl ProjectTrack {
    /// A new track for a WAV file, with uTool2's defaults.
    pub fn new(channel: i32, wav: &str, audio_lib: &str) -> Self {
        let sr = WavInfo::read(wav).map(|i| sr_code(i.rate)).unwrap_or(6);
        ProjectTrack {
            channel,
            audiofile: unresolve(wav, audio_lib),
            vts: mtime_ms(wav),
            sr,
            ati: 0,
            name: Path::new(wav)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            monitor_level: "0.0".into(),
            monitor_pan: "0.0".into(),
            monitor_mute: "0".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Song Project: one .songprj of the project library, read once.
// ---------------------------------------------------------------------------

/// A .songprj as typed values. Callers read fields instead of XML tags; the
/// uTool2 details ({$AudioLib} placeholder, validationcode, song.set layout,
/// rounding, the .songprj layout on save) stay in here.
#[derive(Clone, Debug)]
pub struct SongProject {
    /// where it was loaded from / is saved to; empty for parse()
    pub path: String,
    /// the text as read: to_xml() keeps everything the fields don't cover
    /// (timestamp, markers, sync_units, ...)
    xml: String,
    audio_lib: String,
    /// song_name, decoded. Without one: the file stem (load) or "song" (parse).
    /// It is also the song's folder name on the stick.
    pub name: String,
    pub sampling_rate: u32,
    pub resolution: u32,
    /// in file order; audiofile as stored (normally with {$AudioLib})
    pub tracks: Vec<ProjectTrack>,
    /// as stored, normally with {$AudioLib}; empty = no MIDI file
    pub midi_file: String,
    pub midi_duration: f64,
    pub duration_seconds: f64,
}

impl SongProject {
    /// Read a .songprj. The error names the file.
    pub fn load(path: &str, audio_lib: &str) -> std::io::Result<Self> {
        let xml = fs::read_to_string(path).map_err(|e| {
            std::io::Error::new(e.kind(), format!("song project {}: {}", path, e))
        })?;
        let stem = Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut p = Self::parse_named(&xml, audio_lib, &stem);
        p.path = path.to_string();
        Ok(p)
    }

    /// A .songprj from its text (no file behind it).
    pub fn parse(xml: &str, audio_lib: &str) -> Self {
        Self::parse_named(xml, audio_lib, "song")
    }

    fn parse_named(xml: &str, audio_lib: &str, fallback_name: &str) -> Self {
        let num = |n: &str| tag(xml, n).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let int = |n: &str| tag(xml, n).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
        SongProject {
            path: String::new(),
            name: tag(xml, "song_name")
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| fallback_name.to_string()),
            sampling_rate: int("sampling_rate"),
            resolution: int("resolution"),
            tracks: project_tracks(xml),
            midi_file: tag(xml, "midi_file").unwrap_or_default(),
            midi_duration: num("midi_duration"),
            duration_seconds: num("duration_seconds"),
            xml: xml.to_string(),
            audio_lib: audio_lib.to_string(),
        }
    }

    pub fn audio_lib(&self) -> &str {
        &self.audio_lib
    }

    /// Full path of a track's WAV ({$AudioLib} resolved).
    pub fn wav(&self, track: &ProjectTrack) -> String {
        resolve(&track.audiofile, &self.audio_lib)
    }

    /// Full path of the MIDI file, None if the song has none.
    pub fn midi_path(&self) -> Option<String> {
        if self.midi_file.is_empty() {
            None
        } else {
            Some(resolve(&self.midi_file, &self.audio_lib))
        }
    }

    /// Song length: duration_seconds, or the MIDI length if it is longer.
    /// The same value goes into the validationcode and the .play.
    pub fn duration(&self) -> f64 {
        if self.midi_duration > 0.0 {
            self.duration_seconds.max(self.midi_duration)
        } else {
            self.duration_seconds
        }
    }

    /// midi_length_samples of song.set: cut off like uTool2
    /// ((double)dur * sr), including the float quirks of the same math.
    pub fn midi_length_samples(&self) -> i64 {
        (self.midi_duration * self.sampling_rate as f64) as i64
    }

    /// The validationcode of song.set (MD5 over a little-endian buffer of
    /// format, length, MIDI and every WAV's path hash and mtime).
    pub fn validation_code(&self) -> String {
        hex(&md5(&validation_buffer(self)))
    }

    /// The song.set text for the stick, byte-equal with uTool2.
    pub fn song_set(&self) -> String {
        generate_song_set(self, &self.validation_code())
    }

    /// The .songprj text with the current field values, in uTool2's layout.
    /// Unchanged values give the same bytes as read.
    pub fn to_xml(&self) -> String {
        let mut x = self.xml.clone();
        if tag(&x, "song_name").map(|n| n != self.name).unwrap_or(false) {
            x = set_tag(&x, "song_name", &self.name);
        }
        songprj_set_content(&x, &self.tracks, self.duration_seconds, &self.midi_file, self.midi_duration)
    }

    /// Write to_xml() to `path` (atomically) and make it this project's file.
    pub fn save_as(&mut self, path: &str) -> std::io::Result<()> {
        let text = self.to_xml();
        write_bytes_atomic(Path::new(path), text.as_bytes())
            .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {}", path, e)))?;
        self.xml = text;
        self.path = path.to_string();
        Ok(())
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        let path = self.path.clone();
        self.save_as(&path)
    }
}

/// Tracks of a .songprj (raw, with placeholder) incl. their track names.
fn project_tracks(xml: &str) -> Vec<ProjectTrack> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(pos) = rest.find("<ns0:track ") {
        let after = &rest[pos..];
        let Some(end) = after.find("</ns0:track>") else { break };
        let block = &after[..end];
        rest = &after[end + "</ns0:track>".len()..];
        let get = |n: &str| tag(block, n).unwrap_or_default();
        out.push(ProjectTrack {
            channel: get("channel").parse().unwrap_or(0),
            audiofile: get("audiofile"),
            vts: get("validationtimestamp").parse().unwrap_or(0),
            sr: get("samplingrate").parse().unwrap_or(6),
            ati: get("audiofiletrackindex").parse().unwrap_or(0),
            name: get("trackname"),
            monitor_level: get("monitor_level"),
            monitor_pan: get("monitor_pan"),
            monitor_mute: get("monitor_mute"),
        });
    }
    out
}

/// Seconds with three decimals, rounded half up like uTool2 (3.2215 -> 3.222).
pub fn fmt_seconds(x: f64) -> String {
    format!("{:.3}", ((x * 1000.0) + 1e-6).round() / 1000.0)
}

/// Rewrite tracks, track_count, durations and MIDI file of a .songprj in
/// uTool2's layout; everything else (name, timestamp, markers) stays.
fn songprj_set_content(
    xml: &str,
    tracks: &[ProjectTrack],
    duration_seconds: f64,
    midi_file: &str,
    midi_duration: f64,
) -> String {
    let mut x = set_tag(xml, "duration_seconds", &fmt_seconds(duration_seconds));
    x = set_tag(&x, "track_count", &tracks.len().to_string());
    x = set_tag(&x, "midi_file", midi_file);
    x = set_tag(&x, "midi_duration", &fmt_seconds(midi_duration));
    let mut block: Vec<String> = Vec::new();
    if tracks.is_empty() {
        block.push("  <ns0:tracks/>".into());
    } else {
        block.push("  <ns0:tracks>".into());
        for (i, t) in tracks.iter().enumerate() {
            block.push(format!("    <ns0:track id=\"{}\">", i));
            block.push(format!("      <ns0:channel>{}</ns0:channel>", t.channel));
            block.push(format!("      <ns0:audiofile>{}</ns0:audiofile>", xml_escape(&t.audiofile)));
            block.push(format!("      <ns0:validationtimestamp>{}</ns0:validationtimestamp>", t.vts));
            block.push(format!("      <ns0:samplingrate>{}</ns0:samplingrate>", t.sr));
            block.push(format!("      <ns0:audiofiletrackindex>{}</ns0:audiofiletrackindex>", t.ati));
            block.push(format!("      <ns0:trackname>{}</ns0:trackname>", xml_escape(&t.name)));
            block.push(format!("      <ns0:monitor_level>{}</ns0:monitor_level>", t.monitor_level));
            block.push(format!("      <ns0:monitor_pan>{}</ns0:monitor_pan>", t.monitor_pan));
            block.push(format!("      <ns0:monitor_mute>{}</ns0:monitor_mute>", t.monitor_mute));
            block.push("    </ns0:track>".into());
        }
        block.push("  </ns0:tracks>".into());
    }
    let block = block.join("\r\n");
    // replace "  <ns0:tracks/>" or "  <ns0:tracks> ... </ns0:tracks>"
    let start = x.find("<ns0:tracks/>").or_else(|| x.find("<ns0:tracks>"));
    if let Some(s) = start {
        let line_start = x[..s].rfind('\n').map(|p| p + 1).unwrap_or(s);
        let end = if x[s..].starts_with("<ns0:tracks/>") {
            s + "<ns0:tracks/>".len()
        } else if let Some(e) = x[s..].find("</ns0:tracks>") {
            s + e + "</ns0:tracks>".len()
        } else {
            return x;
        };
        x = format!("{}{}{}", &x[..line_start], block, &x[end..]);
    }
    x
}

/// A single PlaylistSong for a .songprj path (relative to the project
/// library), e.g. for playlists assembled in the GUI.
pub fn playlist_song(rel_path: &str, project_lib: &str) -> PlaylistSong {
    let full = Path::new(project_lib).join(rel_path);
    PlaylistSong {
        name: Path::new(rel_path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        songprj: full.to_string_lossy().into_owned(),
        mode: "wait_for_button_press".to_string(),
        delay: "0".to_string(),
    }
}

/// Song paths (relative to the project library) of a .playlistprj.
/// The {$ProjectLib} placeholder is stripped again, so the paths can be
/// edited in the GUI and written back with generate_playlistprj().
pub fn playlist_song_prjs(xml: &str, project_lib: &str) -> Vec<String> {
    parse_playlist(xml, project_lib)
        .1
        .iter()
        .map(|s| path_key::relative_to(&s.songprj, project_lib).unwrap_or(&s.songprj).to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Import from the "AudioLibrary": <lib>\<Song>\<Date + note>\*.wav + 1 .mid
// ---------------------------------------------------------------------------

/// Compare like the Windows Explorer: digit runs as numbers ("-2-" < "-10-"),
/// case-insensitive.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return Equal,
            (None, _) => return Less,
            (_, None) => return Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut n = String::new();
                    while let Some(c) = it.peek().copied().filter(|c| c.is_ascii_digit()) {
                        n.push(c);
                        it.next();
                    }
                    n.trim_start_matches('0').to_string()
                };
                let (n, m) = (take(&mut x), take(&mut y));
                let o = n.len().cmp(&m.len()).then(n.cmp(&m));
                if o != Equal {
                    return o;
                }
            }
            (Some(c), Some(d)) => {
                if c != d {
                    return c.cmp(&d);
                }
                x.next();
                y.next();
            }
        }
    }
}

/// Library name of an imported song: `<Song>_(<Date folder>)`, the scheme
/// already used in the library (e.g. `_APT_(2025-04-07_RF)`).
pub fn import_song_name(song_folder: &str, date_folder: &str) -> String {
    format!("{}_({})", song_folder, date_folder)
}

#[derive(Clone, Debug, PartialEq)]
pub enum ImportStatus {
    /// no song in the library yet, can be imported
    Missing,
    /// exactly one song in the library covers this folder
    Present(String),
    /// more than one song uses this folder (old songs sharing WAVs)
    Duplicate(Vec<String>),
    /// missing, but the folder can't be imported as it is
    Invalid(String),
}

/// One date folder of the AudioLibrary.
#[derive(Clone, Debug)]
pub struct ImportItem {
    pub song_folder: String,
    pub date_folder: String,
    /// full path of the date folder
    pub dir: String,
    /// library song name, see import_song_name()
    pub name: String,
    /// full paths, in slot order (natural sort)
    pub wavs: Vec<String>,
    pub midi: Option<String>,
    pub rate: u32,
    pub bits: u32,
    /// longest WAV in seconds
    pub duration: f64,
    pub status: ImportStatus,
    /// notes that don't block the import (e.g. "no MIDI file")
    pub notes: Vec<String>,
    /// no song yet, but there is a newer version of this song (name of the
    /// newest one in the library, or of the newest date folder for a song
    /// that is not in the library at all): an old folder, not proposed
    pub older_than: Option<String>,
}

impl ImportItem {
    /// a new date folder (or a new song) that should get a song
    pub fn is_new(&self) -> bool {
        self.status == ImportStatus::Missing && self.older_than.is_none()
    }
}

/// A song of the project library as seen by the import: its name and the
/// folders its WAVs are in (resolved).
#[derive(Clone, Debug, Default)]
pub struct LibrarySong {
    pub name: String,
    /// full path of the .songprj
    pub path: String,
    /// normalized, lower case
    pub wav_dirs: Vec<String>,
    /// which song this is a version of: the song folder of the first WAV
    /// (<Song>\<Dateâ€¦>\x.wav), letters and digits only, lower case.
    /// None if the WAV is not in a date folder.
    pub key: Option<String>,
    /// the date folder ("2026-05-10_RF"), empty if key is None
    pub version: String,
}

/// "_ABCDEFU" and "Abcdefu" are the same song: letters and digits, lower case.
pub fn song_key(song_folder: &str) -> String {
    song_folder.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// Does a folder name start with a date "YYYY-MM-DD"?
pub fn is_date_folder(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() >= 10
        && b[..10].iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() })
}

/// (key, version) of a WAV path: <â€¦>\<Song>\<YYYY-MM-DDâ€¦>\file.wav
pub fn song_version_of(wav: &str) -> Option<(String, String)> {
    let p = Path::new(wav);
    let date = p.parent()?;
    let song = date.parent()?;
    let date_name = date.file_name()?.to_string_lossy().into_owned();
    if !is_date_folder(&date_name) {
        return None;
    }
    let key = song_key(&song.file_name()?.to_string_lossy());
    if key.is_empty() {
        return None;
    }
    Some((key, date_name))
}

/// The newest version of `song` in `library`: same key, a later date folder
/// (natural order). Several songs with that folder: the one named
/// `<Song>_(<Date>)` wins, else the first by name. None = already newest.
pub fn newest_version<'a>(song: &LibrarySong, library: &'a [LibrarySong]) -> Option<&'a LibrarySong> {
    use std::cmp::Ordering::*;
    let key = song.key.as_ref()?;
    let canonical = |s: &LibrarySong| s.name.to_lowercase().ends_with(&format!("_({})", s.version.to_lowercase()));
    let mut best: Option<&LibrarySong> = None;
    for c in library.iter().filter(|c| c.key.as_ref() == Some(key)) {
        if natural_cmp(&c.version, &song.version) != Greater {
            continue;
        }
        best = match best {
            None => Some(c),
            Some(b) => match natural_cmp(&c.version, &b.version) {
                Greater => Some(c),
                Less => Some(b),
                Equal if canonical(c) && !canonical(b) => Some(c),
                Equal => Some(b),
            },
        };
    }
    best
}

/// Read all .songprj of `songs_dir` for the import check.
pub fn library_songs(songs_dir: &Path, audio_lib: &str) -> Vec<LibrarySong> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(songs_dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if !p.extension().map(|x| x.eq_ignore_ascii_case("songprj")).unwrap_or(false) {
            continue;
        }
        let xml = fs::read_to_string(&p).unwrap_or_default();
        let prj = SongProject::parse(&xml, audio_lib);
        let (key, version) = prj
            .tracks
            .first()
            .and_then(|t| song_version_of(&prj.wav(t)))
            .map(|(k, v)| (Some(k), v))
            .unwrap_or((None, String::new()));
        let mut wav_dirs: Vec<String> = prj
            .tracks
            .iter()
            .filter_map(|t| {
                Path::new(&prj.wav(t))
                    .parent()
                    .map(|d| path_key::path_key(&d.to_string_lossy()))
            })
            .collect();
        wav_dirs.sort();
        wav_dirs.dedup();
        out.push(LibrarySong {
            name: p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            path: p.to_string_lossy().into_owned(),
            wav_dirs,
            key,
            version,
        });
    }
    out.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    out
}

fn sorted_dir(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<std::path::PathBuf> = fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    v.sort_by(|a, b| {
        natural_cmp(
            &a.file_name().unwrap_or_default().to_string_lossy(),
            &b.file_name().unwrap_or_default().to_string_lossy(),
        )
    });
    v
}

fn has_ext(p: &Path, exts: &[&str]) -> bool {
    p.extension()
        .map(|x| exts.iter().any(|e| x.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// Scan `import_lib` (<Song>\<Date>\â€¦) and compare every date folder with the
/// library. A folder is covered by a song with the name `<Song>_(<Date>)`
/// (case-insensitive) or by a song that uses WAVs from that folder.
pub fn scan_import(import_lib: &str, library: &[LibrarySong]) -> Result<Vec<ImportItem>, String> {
    let root = Path::new(import_lib);
    if import_lib.trim().is_empty() || !root.is_dir() {
        return Err(format!("AudioLibrary folder not found: \"{}\"", import_lib));
    }
    let mut items = Vec::new();
    for song_dir in sorted_dir(root).into_iter().filter(|p| p.is_dir()) {
        let song_folder = song_dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
        // newest version of this song in the library
        let skey = song_key(&song_folder);
        let newest = library
            .iter()
            .filter(|s| s.key.as_deref() == Some(skey.as_str()))
            .max_by(|a, b| natural_cmp(&a.version, &b.version));
        let first = items.len();
        for date_dir in sorted_dir(&song_dir).into_iter().filter(|p| p.is_dir()) {
            let date_folder = date_dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
            // only date folders (YYYY-MM-DDâ€¦); "Audio", "MixScenes" etc. are skipped
            if !is_date_folder(&date_folder) {
                continue;
            }
            let files = sorted_dir(&date_dir);
            let wavs: Vec<String> = files
                .iter()
                .filter(|p| p.is_file() && has_ext(p, &["wav"]) && !wav_tail::is_backup_wav(p))
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            let midis: Vec<String> = files
                .iter()
                .filter(|p| p.is_file() && has_ext(p, &["mid", "midi"]))
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            let dir = date_dir.to_string_lossy().into_owned();
            let name = import_song_name(&song_folder, &date_folder);
            let key = path_key::path_key(&dir);
            let covering: Vec<String> = library
                .iter()
                .filter(|s| s.name.eq_ignore_ascii_case(&name) || s.wav_dirs.iter().any(|d| *d == key))
                .map(|s| s.name.clone())
                .collect();
            let mut item = ImportItem {
                song_folder: song_folder.clone(),
                date_folder: date_folder.clone(),
                dir,
                name,
                wavs,
                midi: midis.first().cloned(),
                rate: 0,
                bits: 0,
                duration: 0.0,
                status: ImportStatus::Missing,
                notes: Vec::new(),
                older_than: newest
                    .filter(|n| natural_cmp(&n.version, &date_folder) == std::cmp::Ordering::Greater)
                    .map(|n| n.name.clone()),
            };
            let mut problems: Vec<String> = Vec::new();
            for w in &item.wavs {
                match WavInfo::read(w) {
                    Ok(i) => {
                        if item.rate == 0 {
                            item.rate = i.rate;
                            item.bits = i.bits as u32;
                        }
                        let fname = Path::new(w).file_name().unwrap_or_default().to_string_lossy().into_owned();
                        if i.channels != 1 {
                            problems.push(format!("{} is not mono ({} ch)", fname, i.channels));
                        } else if i.rate != item.rate || i.bits as u32 != item.bits {
                            problems.push(format!("{} has a different format ({} Hz / {}-bit)", fname, i.rate, i.bits));
                        }
                        item.duration = item.duration.max(i.seconds());
                    }
                    Err(_) => problems.push(format!(
                        "{} is not a usable WAV file",
                        Path::new(w).file_name().unwrap_or_default().to_string_lossy()
                    )),
                }
            }
            if item.wavs.is_empty() {
                problems.push("no WAV files".into());
            } else if item.wavs.len() > 16 {
                problems.push(format!("{} WAV files, the LP-16 has 16 tracks", item.wavs.len()));
            }
            if midis.len() > 1 {
                problems.push(format!("{} MIDI files, expected one", midis.len()));
            } else if midis.is_empty() {
                item.notes.push("no MIDI file".into());
            } else if midi_duration(&midis[0]).is_none() {
                problems.push("MIDI file is not readable".into());
            }
            if !valid_fat32_name(&item.name) {
                problems.push("folder name contains characters not allowed on the stick".into());
            }
            item.status = match covering.len() {
                0 if problems.is_empty() => ImportStatus::Missing,
                0 => ImportStatus::Invalid(problems.join("; ")),
                1 => ImportStatus::Present(covering[0].clone()),
                _ => ImportStatus::Duplicate(covering),
            };
            items.push(item);
        }
        // a song that is not in the library at all: only its newest date
        // folder is new, the older ones are old folders
        if newest.is_none() {
            let song_items = &mut items[first..];
            if let Some(top) = song_items
                .iter()
                .map(|i| i.date_folder.clone())
                .max_by(|a, b| natural_cmp(a, b))
            {
                let top_name = import_song_name(&song_folder, &top);
                for i in song_items.iter_mut().filter(|i| i.date_folder != top) {
                    i.older_than = Some(top_name.clone());
                }
            }
        }
    }
    Ok(items)
}

/// The .songprj text for an import item: WAVs on slots 1..n in folder order,
/// paths with {$AudioLib} if they are inside the audio file library.
pub fn import_songprj(item: &ImportItem, audio_lib: &str, timestamp_ms: i64) -> String {
    let xml = generate_songprj(&item.name, timestamp_ms, item.rate, item.bits, 1);
    let mut prj = SongProject::parse(&xml, audio_lib);
    prj.tracks = item
        .wavs
        .iter()
        .enumerate()
        .map(|(i, w)| ProjectTrack::new(i as i32, w, audio_lib))
        .collect();
    prj.duration_seconds = item.duration;
    if let Some(m) = &item.midi {
        prj.midi_file = unresolve(m, audio_lib);
        prj.midi_duration = midi_duration(m).unwrap_or(0.0);
    }
    prj.to_xml()
}

/// Write the .songprj files of `items` into `songs_dir`. Existing files are
/// never overwritten. Returns the written paths.
pub fn import_songs(items: &[&ImportItem], songs_dir: &Path, audio_lib: &str) -> Result<Vec<String>, String> {
    let mut written = Vec::new();
    // a fresh project library has no "LP16\Multitrack Songs" yet
    if !items.is_empty() {
        fs::create_dir_all(songs_dir).map_err(|e| format!("{}: {}", songs_dir.display(), e))?;
    }
    for item in items {
        let dest = songs_dir.join(format!("{}.songprj", item.name));
        if dest.exists() {
            return Err(format!("{} already exists (nothing overwritten)", dest.display()));
        }
        let text = import_songprj(item, audio_lib, now_ms());
        write_bytes_atomic(&dest, text.as_bytes()).map_err(|e| format!("{}: {}", dest.display(), e))?;
        written.push(dest.to_string_lossy().into_owned());
    }
    Ok(written)
}

/// Sub folders of a uTool2 project library (as uTool2 creates them).
pub const PROJECT_LIB_DIRS: [&str; 5] = [
    project_lib::SONGS_SUBDIR,
    project_lib::PLAYLISTS_SUBDIR,
    "LP16\\Playlists",
    "LP16\\Stereo Playlists",
    "Stereo Songs",
];

/// Create the project library layout plus the audio / import folders,
/// where missing. Returns the folders that were created.
pub fn init_folders(config: &Config) -> Result<Vec<String>, String> {
    let mut wanted: Vec<std::path::PathBuf> = Vec::new();
    if config.project_lib.trim().is_empty() {
        return Err("The Project Library is not set.".into());
    }
    for d in PROJECT_LIB_DIRS {
        wanted.push(Path::new(&config.project_lib).join(d));
    }
    for d in [&config.audio_lib, &config.import_lib] {
        if !d.trim().is_empty() {
            wanted.push(Path::new(d).to_path_buf());
        }
    }
    let mut created = Vec::new();
    for d in wanted {
        if d.is_dir() {
            continue;
        }
        fs::create_dir_all(&d).map_err(|e| format!("{}: {}", d.display(), e))?;
        let s = d.to_string_lossy().into_owned();
        if !created.contains(&s) {
            created.push(s);
        }
    }
    Ok(created)
}

// ---------------------------------------------------------------------------
// Config file (audio-lib, project-lib, stick root) - simple key = value.
// ---------------------------------------------------------------------------

/// Valid name for a song/playlist that ends up on a FAT32 stick?
/// uTool2 rejects these with "The new Song Name contains invalid
/// characters." / "The new Playlist Name contains invalid characters."
/// (fnd_20). FAT32 forbids " * / : < > ? \ | and control characters.
pub fn valid_fat32_name(name: &str) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    !name.chars().any(|c| {
        matches!(c, '"' | '*' | '/' | ':' | '<' | '>' | '?' | '\\' | '|') || c.is_control()
    })
}

#[derive(Clone)]
pub struct Config {
    /// base folder of everything ("library" in the file). If set, the other
    /// paths are derived from it, see Config::apply_library().
    pub library: String,
    /// replaces {$AudioLib} inside the .songprj
    pub audio_lib: String,
    /// root of the project library, replaces {$ProjectLib}
    pub project_lib: String,
    /// default export destination (stick copy)
    pub stick_root: String,
    /// "AudioLibrary" for the song import: <Song>\<Date + note>\*.wav + .mid
    pub import_lib: String,
    /// name of the cpal audio host of the rehearsal player ("WASAPI");
    /// empty = the default host
    pub audio_host: String,
    /// name of the output device; empty = the default device of the host
    pub audio_device: String,
    /// output channels (1-based) of the left and right side of the stereo mix
    pub audio_out_left: u16,
    pub audio_out_right: u16,
    /// buffer size in frames, 0 = driver default (see `AUDIO_BUFFERS`)
    pub audio_buffer: u32,
    /// name of the MIDI output port of the rehearsal player; empty = off
    pub midi_port: String,
    /// rehearsal player: the next song starts 5 s after the end of a song
    /// (off: it waits for Play)
    pub rehearsal_autoplay: bool,
    /// playlist editor: pause between two songs in seconds (concert length)
    pub set_pause: u32,
    /// the rehearsal mix presets (`mix.*` and `mix_active` in the file)
    pub mixes: mix::MixLibrary,
    /// the stereo MP3 backup export (`backup*` in the file)
    pub backup: backup_mix::BackupSettings,
}

/// The buffer sizes (frames) the settings offer; 0 = driver default.
pub const AUDIO_BUFFERS: [u32; 7] = [0, 64, 128, 256, 512, 1024, 2048];
/// Default pause between two songs of a concert (seconds).
pub const DEFAULT_SET_PAUSE: u32 = 60;
/// Largest pause between two songs accepted (seconds).
pub const MAX_SET_PAUSE: u32 = 600;
/// Largest channel number accepted for `audio_out_left` / `audio_out_right`.
pub const AUDIO_MAX_CHANNEL: u16 = 128;

impl Default for Config {
    fn default() -> Self {
        Config {
            library: String::new(),
            audio_lib: String::new(),
            project_lib: String::new(),
            stick_root: String::new(),
            import_lib: String::new(),
            audio_host: String::new(),
            audio_device: String::new(),
            audio_out_left: 1,
            audio_out_right: 2,
            audio_buffer: 0,
            midi_port: String::new(),
            rehearsal_autoplay: false,
            set_pause: DEFAULT_SET_PAUSE,
            mixes: mix::MixLibrary::default(),
            backup: backup_mix::BackupSettings::default(),
        }
    }
}

/// Folder names below the library base folder.
pub const PROJECT_LIB_NAME: &str = "CymaticProjectLib";
pub const AUDIO_LIB_NAME: &str = "AudioLibrary";

impl Config {
    /// Derive the paths from the base folder:
    /// <library>\CymaticProjectLib = project library,
    /// <library>\AudioLibrary = audio file library ({$AudioLib}) and import
    /// source. Without a base folder the separate paths stay (old files).
    pub fn apply_library(&mut self) {
        let base = self.library.trim();
        if base.trim_end_matches(['\\', '/']).is_empty() {
            return;
        }
        // "D:\" gives "D:\CymaticProjectLib", not the relative "D:CymaticProjectLib"
        self.project_lib = path_key::join_root(base, PROJECT_LIB_NAME);
        self.audio_lib = path_key::join_root(base, AUDIO_LIB_NAME);
        self.import_lib = self.audio_lib.clone();
    }
}

pub fn load_config(path: &std::path::Path) -> Config {
    let mut config = Config::default();
    let mut mixes = mix::MixParser::default();
    let mut backup = backup_mix::BackupParser::default();
    if let Ok(text) = fs::read_to_string(path) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let value = value.trim().trim_matches('"').to_string();
                match key.trim() {
                    "library" => config.library = value,
                    "audio_lib" => config.audio_lib = value,
                    "project_lib" => config.project_lib = value,
                    "stick_root" => config.stick_root = value,
                    "import_lib" => config.import_lib = value,
                    "audio_host" => config.audio_host = value,
                    "audio_device" => config.audio_device = value,
                    "midi_port" => config.midi_port = value,
                    "rehearsal_autoplay" => {
                        config.rehearsal_autoplay = matches!(value.to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on");
                    }
                    // invalid values keep the default
                    "set_pause" => {
                        if let Some(p) = value.parse::<u32>().ok().filter(|p| *p <= MAX_SET_PAUSE) {
                            config.set_pause = p;
                        }
                    }
                    "audio_out_left" => {
                        if let Some(c) = parse_audio_channel(&value) {
                            config.audio_out_left = c;
                        }
                    }
                    "audio_out_right" => {
                        if let Some(c) = parse_audio_channel(&value) {
                            config.audio_out_right = c;
                        }
                    }
                    "audio_buffer" => {
                        if let Some(b) = value.parse::<u32>().ok().filter(|b| AUDIO_BUFFERS.contains(b)) {
                            config.audio_buffer = b;
                        }
                    }
                    k if k == "mix_active" || k.starts_with("mix.") => mixes.feed(k, &value),
                    k if backup_mix::is_backup_key(k) => backup.feed(k, &value),
                    _ => {}
                }
            }
        }
    }
    config.mixes = mixes.finish();
    config.backup = backup.finish();
    config.apply_library();
    config
}

fn parse_audio_channel(value: &str) -> Option<u16> {
    value.parse::<u16>().ok().filter(|c| (1..=AUDIO_MAX_CHANNEL).contains(c))
}

/// With a base folder only `library` is written (the rest is derived),
/// otherwise the separate paths of older files.
pub fn save_config(path: &std::path::Path, config: &Config) -> std::io::Result<()> {
    let paths = if config.library.trim().is_empty() {
        format!(
            "audio_lib = \"{}\"\r\nproject_lib = \"{}\"\r\n\
             # song import: <Song>\\<Date + note>\\*.wav + 1 .mid\r\nimport_lib = \"{}\"\r\n",
            config.audio_lib, config.project_lib, config.import_lib
        )
    } else {
        format!(
            "# base folder: {}\\ = project library, {}\\ = audio files + import\r\nlibrary = \"{}\"\r\n",
            PROJECT_LIB_NAME,
            AUDIO_LIB_NAME,
            config.library.trim()
        )
    };
    let text = format!(
        "{}stick_root = \"{}\"\r\n\
         audio_host = \"{}\"\r\naudio_device = \"{}\"\r\n\
         audio_out_left = \"{}\"\r\naudio_out_right = \"{}\"\r\naudio_buffer = \"{}\"\r\nmidi_port = \"{}\"\r\nrehearsal_autoplay = \"{}\"\r\nset_pause = \"{}\"\r\n{}{}",
        paths,
        config.stick_root,
        config.audio_host,
        config.audio_device,
        config.audio_out_left,
        config.audio_out_right,
        config.audio_buffer,
        config.midi_port,
        config.rehearsal_autoplay,
        config.set_pause,
        config.mixes.to_config_text(),
        config.backup.to_config_text()
    );
    fs::write(path, text.as_bytes())
}

/// Write only the backup export settings into the config file: the file
/// is read again and everything else in it stays as it is on disk.
pub fn save_backup(path: &std::path::Path, backup: &backup_mix::BackupSettings) -> std::io::Result<()> {
    let mut config = load_config(path);
    config.backup = backup.clone();
    save_config(path, &config)
}

/// Write only the pause between two songs (playlist editor) into the
/// config file; everything else in it stays as it is on disk.
pub fn save_set_pause(path: &std::path::Path, pause: u32) -> std::io::Result<()> {
    let mut config = load_config(path);
    config.set_pause = pause.min(MAX_SET_PAUSE);
    save_config(path, &config)
}

/// Write only the rehearsal mix presets into the config file: the file is
/// read again and everything else in it stays as it is on disk.
pub fn save_mixes(path: &std::path::Path, mixes: &mix::MixLibrary) -> std::io::Result<()> {
    let mut config = load_config(path);
    config.mixes = mixes.clone();
    save_config(path, &config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_known_vectors() {
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn hash_code64_known() {
        // h = h*101 + codepoint, 64-bit
        assert_eq!(hash_code64("A"), 65);
        assert_eq!(hash_code64("AB"), 65 * 101 + 66);
    }

    #[test]
    #[ignore = "needs the reference data (LP16_TEST_PROJECT_LIB, LP16_TEST_AUDIO_LIB, LP16_TEST_STICK; cargo test -- --ignored)"]
    fn known_song_validation_code() {
        // The real song from fnd_15. Ignored by default, see require().
        let prj: &str = &ref_prj(r"LP16\Multitrack Songs\_ABCDEFU_(2026-05-10_RF).songprj");
        let audio: &str = &test_env("LP16_TEST_AUDIO_LIB");
        require(&[prj]);
        let p = SongProject::load(prj, audio).unwrap();
        // validationcode from the real song.set on D:
        assert_eq!(p.validation_code(), "4138182317b8da8efaad1b12b8a39ffb");
        // midi_length_samples from the real song.set on D:
        // (178.433 s * 48000 Hz, cut off)
        assert_eq!(p.midi_length_samples(), 8564784);
    }

    #[test]
    #[ignore = "needs the reference data (LP16_TEST_PROJECT_LIB, LP16_TEST_AUDIO_LIB, LP16_TEST_STICK; cargo test -- --ignored)"]
    fn song_editor_roundtrip_and_lengths() {
        // Rewriting all real songs with their own content must give the same
        // bytes; WAV and MIDI lengths must match what uTool2 stored.
        let songs: &str = &ref_prj(r"LP16\Multitrack Songs");
        let audio: &str = &test_env("LP16_TEST_AUDIO_LIB");
        require(&[songs]);
        let rd = fs::read_dir(songs).unwrap();
        let (mut checked_wav, mut checked_midi) = (0, 0);
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x != "songprj").unwrap_or(true) {
                continue;
            }
            let xml = fs::read_to_string(&p).unwrap();
            let prj = SongProject::parse(&xml, audio);
            assert_eq!(prj.to_xml(), xml, "roundtrip {}", p.display());
            let tracks = &prj.tracks;
            let dur = tag(&xml, "duration_seconds").unwrap();
            let midi = tag(&xml, "midi_file").unwrap();
            let mdur = tag(&xml, "midi_duration").unwrap();
            // lengths: only where the files are current (vts == mtime)
            let wavs: Vec<String> = tracks.iter().filter(|t| !t.audiofile.is_empty()).map(|t| resolve(&t.audiofile, audio)).collect();
            if !wavs.is_empty() && tracks.iter().all(|t| t.audiofile.is_empty() || (t.vts != 0 && mtime_ms(&resolve(&t.audiofile, audio)) == t.vts)) {
                let max = wavs.iter().filter_map(|w| WavInfo::read(w).ok()).map(|i| i.seconds()).fold(0.0, f64::max);
                assert_eq!(fmt_seconds(max), dur, "wav length {}", p.display());
                checked_wav += 1;
            }
            let mfull = resolve(&midi, audio);
            if !midi.is_empty() && Path::new(&mfull).exists() {
                let d = midi_duration(&mfull).unwrap();
                assert_eq!(fmt_seconds(d), mdur, "midi length {}", p.display());
                checked_midi += 1;
            }
        }
        eprintln!("checked wav {} midi {}", checked_wav, checked_midi);
    }

    #[test]
    fn unresolve_roundtrip() {
        let lib = r"E:\AUDIO";
        assert_eq!(unresolve(r"e:\audio\x\a.wav", lib), r"{$AudioLib}\x\a.wav");
        assert_eq!(unresolve(r"E:\AUDIO2\a.wav", lib), r"E:\AUDIO2\a.wav");
        assert_eq!(resolve(&unresolve(r"E:\AUDIO\a.wav", lib), lib), r"E:\AUDIO\a.wav");
    }

    #[test]
    #[ignore = "needs the reference data (LP16_TEST_PROJECT_LIB, LP16_TEST_AUDIO_LIB, LP16_TEST_STICK; cargo test -- --ignored)"]
    fn midi_rounding_matches_utool2() {
        // uTool2 rounds midi_length commercially and cuts off
        // midi_length_samples (161.89 * 48000 = 7770719.999... -> 7770719).
        let songs: &str = &ref_prj(r"LP16\Multitrack Songs");
        let cases = [
            ("_BE-MINE_(2026-05-10_RF)", 162i64, 7770720i64 - 1),
            ("_APT_(2026-05-10_RF)", 168, 8040000),
            ("_PHYSICAL_(2026-05-10_RF)", 205, 9835584),
        ];
        for (name, want_len, want_smp) in cases {
            let prj = format!(r"{}\{}.songprj", songs, name);
            require(&[&prj]);
            let xml = fs::read_to_string(&prj).unwrap();
            let sr_raw: u32 = tag(&xml, "sampling_rate")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let midi_duration: f64 = tag(&xml, "midi_duration")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            let midi_length = midi_duration.round() as i64;
            let midi_samples = (midi_duration * sr_raw as f64) as i64;
            assert_eq!(midi_length, want_len, "midi_length for {}", name);
            assert_eq!(midi_samples, want_smp, "midi_length_samples for {}", name);
        }
    }

    #[test]
    #[ignore = "needs the reference data (LP16_TEST_PROJECT_LIB, LP16_TEST_AUDIO_LIB, LP16_TEST_STICK; cargo test -- --ignored)"]
    fn playlist_matches_reference() {
        // Byte-exact comparison against the .play that uTool2 wrote to the
        // real stick on D:. Ignored by default.
        let prj: &str = &ref_prj(r"LP16\Multitrack Playlists\261011_ALLE-SONGS.playlistprj");
        let project_lib: &str = &test_env("LP16_TEST_PROJECT_LIB");
        let reference: &str = &ref_stick(r"Recording\#PLAYLISTS\261011_ALLE-SONGS.play");
        require(&[prj, reference]);
        let xml = fs::read_to_string(prj).unwrap();
        let (name, songs) = parse_playlist(&xml, project_lib);
        // The timestamp of the real export, so that the comparison is
        // byte-exact including the timestamp.
        let timestamp = 1793538814877;
        let text = generate_play(&name, timestamp, &songs, |song| {
            SongProject::load(&song.songprj, "").unwrap().duration()
        });
        let reference_text = fs::read_to_string(reference).unwrap();
        assert_eq!(
            text,
            reference_text,
            "generated .play differs from the reference on D:"
        );
        // Guard: the test must actually have compared a real playlist.
        assert!(songs.len() >= 25, "only {} songs in the playlist", songs.len());
    }

    #[test]
    #[ignore = "needs the reference data (LP16_TEST_PROJECT_LIB, LP16_TEST_AUDIO_LIB, LP16_TEST_STICK; cargo test -- --ignored)"]
    fn export_wav_rebuilds_utool2_header() {
        // Ignored by default. The reference is the
        // real stick file: same layout, same payload.
        let src: &str = &ref_audio(r"_ABCDEFU\2026-05-10_RF\2026-05-10_RF-1-Klick-7.wav");
        let stick: &str = &ref_stick(r"Recording\_ABCDEFU_(2026-05-10_RF)\_2026-05-10_RF-1-Klick-7_01.wav");
        require(&[src, stick]);
        let out = std::env::temp_dir().join("lp16-tool-test.wav");
        export_wav(src, out.to_str().unwrap()).unwrap();

        let ours = fs::read(&out).unwrap();
        let reference = fs::read(stick).unwrap();
        assert_eq!(ours.len(), reference.len(), "file size differs");
        // Header layout: RIFF size, JUNK(52), fmt(16), data offset, data size.
        assert_eq!(ours[0..4], reference[0..4]); // "RIFF"
        assert_eq!(ours[4..8], reference[4..8]); // RIFF size
        assert_eq!(ours[8..12], reference[8..12]); // "WAVE"
        assert_eq!(ours[12..72], reference[12..72]); // JUNK chunk
        assert_eq!(ours[72..96], reference[72..96]); // fmt chunk
        assert_eq!(ours[96..104], reference[96..104]); // data chunk header
        // PCM payload byte-identical (no gain processing).
        assert_eq!(ours[104..], reference[104..]);
        // Naming: stem + two-digit channel, 1-based like uTool2
        // (songprj channel 0 -> "â€¦_01.wav").
        assert_eq!(
            stick_wav_name(src, 0),
            "_2026-05-10_RF-1-Klick-7_01.wav"
        );
    }

    #[test]
    fn fat32_name_validation() {
        // uTool2 rejects invalid characters (fnd_20); the stick is FAT32.
        assert!(valid_fat32_name("261011_ALLE-SONGS"));
        assert!(valid_fat32_name("SUCH A SHAME_(2026-05-10_RF)"));
        assert!(!valid_fat32_name("a<b"));
        assert!(!valid_fat32_name("a:b"));
        assert!(!valid_fat32_name("a*b?c"));
        assert!(!valid_fat32_name("a|b"));
        assert!(!valid_fat32_name("a\\b"));
        assert!(!valid_fat32_name("a/b\"c"));
        assert!(!valid_fat32_name(""));       // empty
        assert!(!valid_fat32_name("   "));    // only spaces
    }

    #[test]
    #[ignore = "needs the reference data (LP16_TEST_PROJECT_LIB, LP16_TEST_AUDIO_LIB, LP16_TEST_STICK; cargo test -- --ignored)"]
    fn all_stick_songs_match_reference() {
        // Byte-exact comparison against every song.set that uTool2 wrote
        // to the real stick on D:. Ignored by default.
        let songs: &str = &ref_prj(r"LP16\Multitrack Songs");
        let audio: &str = &test_env("LP16_TEST_AUDIO_LIB");
        let stick: &str = &ref_stick("Recording");
        require(&[songs, stick]);
        let mut checked = 0usize;
        let mut mismatches: Vec<String> = Vec::new();
        for entry in fs::read_dir(stick).unwrap() {
            let dir = entry.unwrap().path();
            if !dir.is_dir() {
                continue;
            }
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            let prj = Path::new(songs).join(format!("{}.songprj", name));
            let reference = dir.join("song.set");
            if !prj.exists() || !reference.exists() {
                continue;
            }
            let text = SongProject::load(&prj.to_string_lossy(), audio).unwrap().song_set();
            let reference_text = fs::read_to_string(&reference).unwrap();
            checked += 1;
            if text != reference_text {
                mismatches.push(name);
            }
        }
        // Guard against the test silently doing nothing.
        assert!(checked >= 25, "only {} songs checked", checked);
        assert!(
            mismatches.is_empty(),
            "songs that differ from the reference: {:?}",
            mismatches
        );
    }

    // -----------------------------------------------------------------------
    // Self-contained tests (no E:/D: needed): streaming WAV export, atomic
    // writing, config.
    // -----------------------------------------------------------------------

    /// Paths of the reference data for the #[ignore]d tests come from
    /// environment variables, not from the code:
    ///   LP16_TEST_PROJECT_LIB  project library with the reference songs/playlists
    ///   LP16_TEST_AUDIO_LIB    its audio library ({$AudioLib})
    ///   LP16_TEST_STICK        root of a stick written by uTool2 (e.g. D:\)
    fn test_env(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| {
            panic!("{} is not set (path of the reference data for cargo test -- --ignored, see README)", name)
        })
    }

    fn ref_prj(rel: &str) -> String {
        Path::new(&test_env("LP16_TEST_PROJECT_LIB")).join(rel).to_string_lossy().into_owned()
    }

    fn ref_audio(rel: &str) -> String {
        Path::new(&test_env("LP16_TEST_AUDIO_LIB")).join(rel).to_string_lossy().into_owned()
    }

    fn ref_stick(rel: &str) -> String {
        Path::new(&test_env("LP16_TEST_STICK")).join(rel).to_string_lossy().into_owned()
    }

    /// Tests against the real library / stick are #[ignore]d. When they are
    /// run explicitly (cargo test -- --ignored), missing data is a failure
    /// and not a silent "ok".
    fn require(paths: &[&str]) {
        for p in paths {
            assert!(Path::new(p).exists(), "test data missing: {} (drive not connected?)", p);
        }
    }

    // -----------------------------------------------------------------------
    // XML entities
    // -----------------------------------------------------------------------

    #[test]
    fn xml_entities_are_decoded_and_escaped_once() {
        assert_eq!(xml_unescape("Rock &amp; Roll"), "Rock & Roll");
        assert_eq!(xml_unescape("&lt;a&gt; &quot;b&quot; &apos;c&apos;"), "<a> \"b\" 'c'");
        assert_eq!(xml_unescape("&#38;&#x26;&#X26;"), "&&&");
        // unknown or broken entities stay untouched
        assert_eq!(xml_unescape("a &nbsp; b & c &amp"), "a &nbsp; b & c &amp");
        assert_eq!(xml_unescape("plain"), "plain");

        let xml = "<ns0:SongProject><ns0:song_name>Rock &amp; Roll</ns0:song_name></ns0:SongProject>";
        assert_eq!(tag(xml, "song_name").unwrap(), "Rock & Roll");
        let x = set_tag(xml, "song_name", "A & <B>");
        assert!(x.contains("<ns0:song_name>A &amp; &lt;B&gt;</ns0:song_name>"), "{}", x);
        assert_eq!(tag(&x, "song_name").unwrap(), "A & <B>");
        assert_eq!(plain_tag("<name>x &amp; y</name>", "name").unwrap(), "x & y");

        // song.set: escaped exactly once, not "&amp;amp;"
        let set = SongProject::parse(xml, "").song_set();
        assert!(set.contains("<song_name>Rock &amp; Roll</song_name>"), "{}", set);
        // .play: playlist name and song name escaped once
        let songs = [PlaylistSong { name: "R & R".into(), songprj: String::new(), mode: "m".into(), delay: "0".into() }];
        let play = generate_play("A & B", 0, &songs, |_| 1.0);
        assert!(play.contains("<playlistname>A &amp; B</playlistname>"));
        assert!(play.contains("<name>R &amp; R</name>"));
    }

    #[test]
    fn start_modes_are_the_utool2_values() {
        for m in StartMode::ALL {
            assert_eq!(StartMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(StartMode::parse(""), Some(StartMode::Button));
        assert_eq!(StartMode::parse("auto_start"), None);
        assert_eq!((delay_seconds("5"), delay_seconds(" 2,6 "), delay_seconds(""), delay_seconds("-3"), delay_seconds("x")), (5, 3, 0, 0, 0));
        assert_eq!(start_mode_text("wait_period", "7"), "Wait for timer (7 s)");
        assert_eq!(start_mode_text("no_wait", "0"), "No wait");
        assert_eq!(start_mode_text("odd", "0"), "Unknown start mode \"odd\"");
    }

    #[test]
    fn start_mode_and_delay_reach_the_playlistprj_and_the_play() {
        let songs = [PlaylistSong { name: "A".into(), songprj: r"C:\lib\A.songprj".into(), mode: "wait_period".into(), delay: "5".into() }];
        let prj = generate_playlistprj("P", 0, r"C:\lib", &songs);
        assert!(prj.contains("<ns0:start_mode>wait_period</ns0:start_mode>"), "{}", prj);
        assert!(prj.contains("<ns0:start_mode_delay>5</ns0:start_mode_delay>"), "{}", prj);
        let (_, back) = parse_playlist(&prj, r"C:\lib");
        assert_eq!((back[0].mode.as_str(), back[0].delay.as_str()), ("wait_period", "5"));
        let play = generate_play("P", 0, &songs, |_| 1.0);
        assert!(play.contains("<mode>wait_period</mode>") && play.contains("<time>5</time>"), "{}", play);
    }

    #[test]
    fn song_set_has_midi_block_only_with_midi_file() {
        // uTool2 leaves the MIDI tags out when the song has no MIDI file
        let without = SongProject::parse("<ns0:SongProject><ns0:song_name>A</ns0:song_name></ns0:SongProject>", "")
            .song_set();
        for t in ["midi_file", "midi_length", "midi_length_samples", "midi_offset"] {
            assert!(!without.contains(&format!("<{}>", t)), "{} in {}", t, without);
        }

        let with = SongProject::parse(
            "<ns0:SongProject><ns0:song_name>A</ns0:song_name><ns0:midi_file>{$AudioLib}\\x\\song.mid</ns0:midi_file></ns0:SongProject>",
            "",
        )
        .song_set();
        assert!(with.contains("  <midi_file>song.mid</midi_file>\r\n  <midi_length>"), "{}", with);
        assert!(with.contains("<midi_offset>0</midi_offset>"), "{}", with);
    }

    // -----------------------------------------------------------------------
    // plan_song with a small library in a temp folder
    // -----------------------------------------------------------------------

    /// Writes a .songprj with two mono WAVs and a MIDI file below <root>\AUDIO.
    pub(crate) fn mini_song(root: &Path, name: &str) -> (String, String) {
        let audio = root.join("AUDIO");
        let src = audio.join("Song").join("v1");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a-1-klick.wav"), mono_wav(4800, 48000, 24, 1)).unwrap();
        fs::write(src.join("a-2-bass.wav"), mono_wav(9600, 48000, 24, 1)).unwrap();
        fs::write(src.join("song.mid"), tiny_midi()).unwrap();
        let audio = audio.to_string_lossy().into_owned();
        let tracks: Vec<ProjectTrack> = ["a-1-klick.wav", "a-2-bass.wav"]
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let full = src.join(f).to_string_lossy().into_owned();
                ProjectTrack {
                    channel: i as i32,
                    audiofile: unresolve(&full, &audio),
                    vts: mtime_ms(&full),
                    sr: 2,
                    ati: 0,
                    name: f.to_string(),
                    monitor_level: "0".into(),
                    monitor_pan: "0".into(),
                    monitor_mute: "false".into(),
                }
            })
            .collect();
        let mut prj = SongProject::parse(&generate_songprj(name, 1, 48000, 24, 0), &audio);
        prj.tracks = tracks;
        prj.duration_seconds = 0.2;
        prj.midi_file = unresolve(&src.join("song.mid").to_string_lossy(), &audio);
        prj.midi_duration = 0.5;
        (prj.to_xml(), audio)
    }

    #[test]
    fn plan_song_detects_every_reason() {
        let root = temp_dir("plan");
        let (xml, audio) = mini_song(&root, "Rock & Roll");
        let stick = root.join("STICK");
        let stick_s = stick.to_string_lossy().into_owned();
        let plan = || plan_song(&SongProject::parse(&xml, &audio), &stick_s);
        let export = || export_plan(&plan(), &stick_s).unwrap();

        let p = plan();
        assert_eq!((p.needs_export, p.reason.as_str()), (true, "new song"));
        // decoded name -> real folder name, no "&amp;"
        assert_eq!(p.name, "Rock & Roll");
        export();
        let dir = stick.join("Rock & Roll");
        assert!(dir.join("song.set").is_file());
        assert_eq!(plan().reason, "up to date");
        assert!(!plan().needs_export);

        // a WAV deleted on the stick
        let (_, dest0) = p.wav[0].clone();
        fs::remove_file(&dest0).unwrap();
        assert_eq!(plan().reason, "1 wav file(s) missing");
        export();
        assert_eq!(plan().reason, "up to date");

        // a source WAV with a different length
        let (src1, _) = p.wav[1].clone();
        fs::write(&src1, mono_wav(14400, 48000, 24, 1)).unwrap();
        assert_eq!(plan().reason, "1 wav file(s) changed on source");
        export();
        assert_eq!(plan().reason, "up to date");

        // the MIDI file deleted on the stick
        let (midi_src, midi_dest) = p.midi.clone().unwrap();
        fs::remove_file(&midi_dest).unwrap();
        assert_eq!(plan().reason, "midi file missing");
        export();
        assert_eq!(plan().reason, "up to date");

        // a changed MIDI file (same size, new mtime) only shows up in the
        // validationcode of song.set
        let f = fs::File::options().write(true).open(&midi_src).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60)).unwrap();
        drop(f);
        assert_eq!(plan().reason, "song.set changed");
        export();
        assert_eq!(plan().reason, "up to date");

        // song.set deleted
        fs::remove_file(dir.join("song.set")).unwrap();
        assert_eq!(plan().reason, "song.set missing");
        export();

        // a source WAV that cannot be read
        fs::remove_file(&src1).unwrap();
        assert_eq!(plan().reason, "1 source wav file(s) unreadable");

        let _ = fs::remove_dir_all(&root);
    }

    pub(crate) fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lp16-tool-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut c = Vec::new();
        c.extend_from_slice(id);
        c.extend_from_slice(&(body.len() as u32).to_le_bytes());
        c.extend_from_slice(body);
        if body.len() % 2 == 1 {
            c.push(0);
        }
        c
    }

    fn fmt_body(channels: u16, rate: u32, bits: u16, extra: usize) -> Vec<u8> {
        let align = channels * bits / 8;
        let mut b = Vec::new();
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * align as u32).to_le_bytes());
        b.extend_from_slice(&align.to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b.extend(std::iter::repeat(0u8).take(extra)); // e.g. WAVE_FORMAT_EXTENSIBLE tail
        b
    }

    fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = chunks.concat();
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(4 + body.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVE");
        w.extend_from_slice(&body);
        w
    }

    /// The previous in-memory export_wav, kept as the reference. Takes the
    /// format the test case was built with, so it does not share the reader.
    fn export_wav_in_memory(channels: u16, rate: u32, bits: u16, payload: &[u8]) -> Vec<u8> {
        let len = payload.len();
        let pad = (len % 2) as u32;
        let align = channels * bits / 8;
        let riff_size = 4u32 + 60 + 24 + (8 + len as u32 + pad);
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&riff_size.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"JUNK");
        out.extend_from_slice(&52u32.to_le_bytes());
        out.extend_from_slice(&[0u8; 52]);
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * align as u32).to_le_bytes());
        out.extend_from_slice(&align.to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(len as u32).to_le_bytes());
        out.extend_from_slice(payload);
        if pad == 1 {
            out.push(0);
        }
        out
    }

    #[test]
    fn streaming_wav_export_matches_in_memory_version() {
        let dir = temp_dir("wav");
        let payload_even: Vec<u8> = (0..30_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let payload_odd: Vec<u8> = (0..30_001u32).map(|i| (i * 13 % 241) as u8).collect();
        let cases: Vec<(&str, Vec<u8>, (u16, u32, u16), &[u8])> = vec![
            (
                "plain",
                riff(&[chunk(b"fmt ", &fmt_body(1, 48000, 24, 0)), chunk(b"data", &payload_even)]),
                (1, 48000, 24),
                &payload_even,
            ),
            (
                "odd",
                riff(&[chunk(b"fmt ", &fmt_body(1, 44100, 24, 0)), chunk(b"data", &payload_odd)]),
                (1, 44100, 24),
                &payload_odd,
            ),
            (
                "extra_chunks",
                riff(&[
                    chunk(b"JUNK", &[0u8; 28]),
                    chunk(b"bext", &[1u8; 603]),
                    chunk(b"fmt ", &fmt_body(1, 48000, 16, 24)),
                    chunk(b"data", &payload_even),
                    chunk(b"LIST", &[2u8; 11]),
                ]),
                (1, 48000, 16),
                &payload_even,
            ),
            (
                "data_before_fmt",
                riff(&[chunk(b"data", &payload_odd), chunk(b"fmt ", &fmt_body(1, 96000, 32, 0))]),
                (1, 96000, 32),
                &payload_odd,
            ),
        ];
        for (name, bytes, (channels, rate, bits), payload) in cases {
            let src = dir.join(format!("{}.wav", name));
            let dest = dir.join(format!("{}_out.wav", name));
            fs::write(&src, &bytes).unwrap();
            export_wav(src.to_str().unwrap(), dest.to_str().unwrap()).unwrap();
            let ours = fs::read(&dest).unwrap();
            assert_eq!(ours, export_wav_in_memory(channels, rate, bits, payload), "case {}", name);
            assert_eq!(
                ours.len() as u64,
                WavInfo::read(src.to_str().unwrap()).unwrap().stick_size(),
                "case {}",
                name
            );
            assert!(!tmp_path(&dest).exists(), "tmp file left behind ({})", name);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wav_info_reads_every_layout_and_refuses_broken_files() {
        let dir = temp_dir("wavinfo");
        let read = |name: &str, bytes: Vec<u8>| {
            let p = dir.join(format!("{}.wav", name));
            fs::write(&p, bytes).unwrap();
            WavInfo::read(p.to_str().unwrap())
        };
        let odd = [7u8; 1001];
        // fmt before data, with chunks around them; odd chunks are padded
        let i = read(
            "fmt_first",
            riff(&[chunk(b"bext", &[1u8; 5]), chunk(b"fmt ", &fmt_body(1, 48000, 24, 0)), chunk(b"data", &odd)]),
        )
        .unwrap();
        // 12 RIFF + 8+5+1 bext + 8+16 fmt + 8 data header
        assert_eq!(
            i,
            WavInfo { channels: 1, rate: 48000, bits: 24, format: SampleFormat::Int, data_off: 58, data_len: 1001 }
        );
        assert_eq!(i.stick_size(), STICK_WAV_HEADER_LEN + 1002);
        // data before fmt: the same file in the other order
        let i = read("data_first", riff(&[chunk(b"data", &odd), chunk(b"fmt ", &fmt_body(2, 44100, 16, 0))])).unwrap();
        assert_eq!((i.channels, i.rate, i.bits, i.data_off, i.data_len), (2, 44100, 16, 20, 1001));
        // WAVE_FORMAT_EXTENSIBLE: longer fmt chunk
        assert_eq!(read("ext", riff(&[chunk(b"fmt ", &fmt_body(1, 96000, 32, 24)), chunk(b"data", &odd)])).unwrap().bits, 32);
        // refused: no fmt, no data, empty format, data longer than the file, no RIFF
        let fmt = || chunk(b"fmt ", &fmt_body(1, 48000, 24, 0));
        let broken: Vec<(&str, Vec<u8>)> = vec![
            ("no_fmt", riff(&[chunk(b"data", &odd)])),
            ("no_data", riff(&[fmt()])),
            ("zero_channels", riff(&[chunk(b"fmt ", &fmt_body(0, 48000, 24, 0)), chunk(b"data", &odd)])),
            ("short_fmt", riff(&[chunk(b"fmt ", &[1u8; 14]), chunk(b"data", &odd)])),
            ("truncated", {
                let mut b = riff(&[fmt(), chunk(b"data", &odd)]);
                b.truncate(b.len() - 100);
                b
            }),
            ("not_riff", b"RIFX\0\0\0\0WAVE".to_vec()),
            ("too_short", b"RIFF".to_vec()),
        ];
        for (name, bytes) in broken {
            let e = read(name, bytes).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "case {}", name);
        }
        // a missing file stays a NotFound error
        assert_eq!(WavInfo::read(dir.join("gone.wav").to_str().unwrap()).unwrap_err().kind(), std::io::ErrorKind::NotFound);
        let _ = fs::remove_dir_all(&dir);
    }

    /// fmt chunk body with an explicit format tag (and optional EXTENSIBLE
    /// tail: 24 bytes, sub-format GUID starts with `sub_tag`).
    fn fmt_tagged(tag: u16, channels: u16, rate: u32, bits: u16, sub_tag: Option<u16>) -> Vec<u8> {
        let mut b = fmt_body(channels, rate, bits, 0);
        b[0..2].copy_from_slice(&tag.to_le_bytes());
        if let Some(sub) = sub_tag {
            b.extend_from_slice(&[22, 0]); // cbSize
            b.extend_from_slice(&[bits as u8, 0]); // valid bits
            b.extend_from_slice(&[0u8; 4]); // channel mask
            b.extend_from_slice(&sub.to_le_bytes()); // GUID starts with the tag
            b.extend_from_slice(&[0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71]);
        }
        b
    }

    fn write_wav(dir: &Path, name: &str, fmt: Vec<u8>, payload: &[u8]) -> String {
        let p = dir.join(format!("{}.wav", name));
        fs::write(&p, riff(&[chunk(b"fmt ", &fmt), chunk(b"data", payload)])).unwrap();
        p.to_str().unwrap().to_string()
    }

    fn read_all(r: &mut WavReader) -> Vec<f32> {
        let mut out = vec![0f32; r.frames() as usize * r.channels() as usize];
        assert_eq!(r.read(&mut out), r.frames() as usize);
        out
    }

    #[test]
    fn wav_info_reports_sample_format() {
        let dir = temp_dir("wavformat");
        let fmt_of = |name: &str, fmt: Vec<u8>| WavInfo::read(&write_wav(&dir, name, fmt, &[0u8; 8])).unwrap().format;
        assert_eq!(fmt_of("pcm", fmt_tagged(1, 1, 48000, 16, None)), SampleFormat::Int);
        assert_eq!(fmt_of("float", fmt_tagged(3, 1, 48000, 32, None)), SampleFormat::Float);
        assert_eq!(fmt_of("ext_int", fmt_tagged(0xFFFE, 1, 48000, 24, Some(1))), SampleFormat::Int);
        assert_eq!(fmt_of("ext_float", fmt_tagged(0xFFFE, 1, 48000, 32, Some(3))), SampleFormat::Float);
        assert_eq!(fmt_of("ext_other", fmt_tagged(0xFFFE, 1, 48000, 32, Some(2))), SampleFormat::Unknown);
        // EXTENSIBLE with a fmt chunk shorter than 40 bytes: unknown
        let mut short = fmt_tagged(0xFFFE, 1, 48000, 32, None);
        short.extend_from_slice(&[0u8; 8]);
        assert_eq!(fmt_of("ext_short", short), SampleFormat::Unknown);
        assert_eq!(fmt_of("adpcm", fmt_tagged(2, 1, 48000, 16, None)), SampleFormat::Unknown);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wav_reader_converts_every_sample_format() {
        let dir = temp_dir("wavreader_formats");
        let open = |name: &str, fmt: Vec<u8>, payload: Vec<u8>| WavReader::open(&write_wav(&dir, name, fmt, &payload)).unwrap();
        // 0, max, min, half scale (exactly representable in f32)
        let expect = [0.0f32, 1.0 - 1.0 / 128.0, -1.0, 0.5];
        let mut r = open("u8", fmt_tagged(1, 1, 48000, 8, None), vec![128, 255, 0, 192]);
        assert_eq!(read_all(&mut r), [0.0, 127.0 / 128.0, -1.0, 0.5]);
        let p16: Vec<u8> = [0i16, 32767, -32768, 16384].iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut r = open("i16", fmt_tagged(1, 1, 48000, 16, None), p16);
        assert_eq!(read_all(&mut r), [0.0, 32767.0 / 32768.0, -1.0, 0.5]);
        let p24: Vec<u8> = [0i32, 8388607, -8388608, 4194304].iter().flat_map(|v| v.to_le_bytes()[..3].to_vec()).collect();
        let mut r = open("i24", fmt_tagged(1, 1, 48000, 24, None), p24);
        assert_eq!(read_all(&mut r), [0.0, 8388607.0 / 8388608.0, -1.0, 0.5]);
        let p32: Vec<u8> = [0i32, i32::MAX, i32::MIN, 1 << 30].iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut r = open("i32", fmt_tagged(1, 1, 48000, 32, None), p32);
        let got = read_all(&mut r);
        assert_eq!((got[0], got[2], got[3]), (0.0, -1.0, 0.5));
        assert!((got[1] - 1.0).abs() < 1e-6);
        let pf: Vec<u8> = expect.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut r = open("f32", fmt_tagged(3, 1, 48000, 32, None), pf.clone());
        assert_eq!(read_all(&mut r), expect);
        // EXTENSIBLE with a float sub-format is read as float
        let mut r = open("ext_f32", fmt_tagged(0xFFFE, 1, 48000, 32, Some(3)), pf);
        assert_eq!(read_all(&mut r), expect);
        assert_eq!((r.channels(), r.frames(), r.rate()), (1, 4, 48000));
        // interleaved stereo
        let p: Vec<u8> = [1000i16, -1000, 2000, -2000].iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut r = open("stereo", fmt_tagged(1, 2, 44100, 16, None), p);
        assert_eq!((r.channels(), r.frames()), (2, 2));
        assert_eq!(read_all(&mut r), [1000.0 / 32768.0, -1000.0 / 32768.0, 2000.0 / 32768.0, -2000.0 / 32768.0]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wav_reader_seeks_and_stops_at_the_end() {
        let dir = temp_dir("wavreader_seek");
        // ramp: the value of frame n is n / 32768; long enough for several refills
        let n = 70_000u32;
        let payload: Vec<u8> = (0..n).flat_map(|i| (i as i32 as f32 / 65536.0).to_le_bytes()).collect();
        let path = write_wav(&dir, "ramp", fmt_tagged(3, 1, 48000, 32, None), &payload);
        let mut r = WavReader::open(&path).unwrap();
        assert_eq!(r.frames(), n as u64);
        let mut buf = [0f32; 10];
        for start in [0u32, 5, 65_535, 65_536, 69_000] {
            r.seek(start as u64).unwrap();
            assert_eq!(r.read(&mut buf), 10);
            for (k, v) in buf.iter().enumerate() {
                assert_eq!(*v, (start + k as u32) as f32 / 65536.0, "start {} k {}", start, k);
            }
        }
        // a read across the refill boundary
        r.seek(0).unwrap();
        let mut big = vec![0f32; 70_000];
        assert_eq!(r.read(&mut big), 70_000);
        assert_eq!(big[69_999], 69_999.0 / 65536.0);
        // over the end: the remaining frames, then 0
        r.seek(n as u64 - 3).unwrap();
        assert_eq!(r.read(&mut buf), 3);
        assert_eq!(buf[..3], [(n - 3) as f32 / 65536.0, (n - 2) as f32 / 65536.0, (n - 1) as f32 / 65536.0]);
        assert_eq!(r.read(&mut buf), 0);
        assert_eq!(r.read(&mut buf), 0);
        // seek after the end is clamped; seek back works
        r.seek(n as u64 + 100).unwrap();
        assert_eq!(r.read(&mut buf), 0);
        r.seek(7).unwrap();
        assert_eq!(r.read(&mut buf[..1]), 1);
        assert_eq!(buf[0], 7.0 / 65536.0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wav_reader_refuses_unsupported_formats_that_wav_info_accepts() {
        let dir = temp_dir("wavreader_unsupported");
        let f64_bytes = [0u8; 16];
        let cases = [
            ("float64", fmt_tagged(3, 1, 48000, 64, None)),
            ("adpcm", fmt_tagged(2, 1, 48000, 16, None)),
            ("ext_short", fmt_tagged(0xFFFE, 1, 48000, 32, None)),
            ("int12", fmt_tagged(1, 1, 48000, 12, None)),
        ];
        for (name, fmt) in cases {
            let path = write_wav(&dir, name, fmt, &f64_bytes);
            assert!(WavInfo::read(&path).is_ok(), "case {}", name);
            let e = WavReader::open(&path).err().unwrap_or_else(|| panic!("case {} opened", name));
            let msg = e.to_string();
            assert!(msg.contains("unsupported WAV format") && msg.contains(&format!("{}.wav", name)), "case {}: {}", name, msg);
        }
        // a missing file: the error names it
        let e = WavReader::open(dir.join("gone.wav").to_str().unwrap()).err().unwrap();
        assert!(e.to_string().contains("gone.wav"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wav_reader_handles_data_before_fmt_and_extra_chunks() {
        let dir = temp_dir("wavreader_layout");
        let payload: Vec<u8> = [100i16, -200, 300].iter().flat_map(|v| v.to_le_bytes()).collect();
        let expect = [100.0 / 32768.0, -200.0 / 32768.0, 300.0 / 32768.0];
        let layouts = [
            ("data_first", vec![chunk(b"data", &payload), chunk(b"fmt ", &fmt_tagged(1, 1, 48000, 16, None))]),
            (
                "list_around",
                vec![
                    chunk(b"LIST", &[9u8; 7]),
                    chunk(b"fmt ", &fmt_tagged(1, 1, 48000, 16, None)),
                    chunk(b"LIST", &[1u8; 11]),
                    chunk(b"data", &payload),
                    chunk(b"LIST", &[2u8; 5]),
                ],
            ),
        ];
        for (name, chunks) in layouts {
            let p = dir.join(format!("{}.wav", name));
            fs::write(&p, riff(&chunks)).unwrap();
            let mut r = WavReader::open(p.to_str().unwrap()).unwrap();
            assert_eq!(read_all(&mut r), expect, "case {}", name);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wav_without_fmt_is_not_exported() {
        let dir = temp_dir("nofmt");
        let src = dir.join("nofmt.wav");
        let dest = dir.join("out.wav");
        fs::write(&src, riff(&[chunk(b"data", &[5u8; 100])])).unwrap();
        assert!(export_wav(src.to_str().unwrap(), dest.to_str().unwrap()).is_err());
        assert!(!dest.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncated_wav_is_refused_and_keeps_old_file() {
        let dir = temp_dir("trunc");
        let mut bytes = riff(&[chunk(b"fmt ", &fmt_body(1, 48000, 24, 0)), chunk(b"data", &[5u8; 1000])]);
        bytes.truncate(bytes.len() - 100); // data chunk claims more than the file has
        let src = dir.join("broken.wav");
        let dest = dir.join("out.wav");
        fs::write(&src, &bytes).unwrap();
        fs::write(&dest, b"old content").unwrap();
        assert!(export_wav(src.to_str().unwrap(), dest.to_str().unwrap()).is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"old content");
        assert!(!tmp_path(&dest).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_replaces_and_cleans_up_on_error() {
        let dir = temp_dir("atomic");
        let dest = dir.join("song.set");
        write_bytes_atomic(&dest, b"first").unwrap();
        write_bytes_atomic(&dest, b"second").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"second");
        // a failing writer must leave the old file and no tmp file
        let r = write_atomic(&dest, |w| {
            use std::io::Write;
            w.write_all(b"half")?;
            Err(std::io::Error::new(std::io::ErrorKind::Other, "stick pulled"))
        });
        assert!(r.is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"second");
        assert!(!tmp_path(&dest).exists());
        // leftovers of a crashed export are removed
        fs::write(dir.join("_x_01.wav.lp16tmp"), b"junk").unwrap();
        remove_tmp_files(&dir);
        assert!(!dir.join("_x_01.wav.lp16tmp").exists());
        assert!(dest.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_roundtrip() {
        let dir = temp_dir("config");
        let path = dir.join("lp16-tool.toml");
        // an old file with protected_targets still loads; the key is dropped
        fs::write(&path, "audio_lib = \"A\"\r\nprotected_targets = \"G:\"\r\n").unwrap();
        assert_eq!(load_config(&path).audio_lib, "A");
        let with_import = Config {
            import_lib: "E:\\x\\AKTIVE-SONGS".to_string(),
            ..Config::default()
        };
        save_config(&path, &with_import).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("protected_targets"));
        assert_eq!(load_config(&path).import_lib, "E:\\x\\AKTIVE-SONGS");
        // base folder: everything else is derived, also after loading
        let mut base = Config { library: "E:\\x\\LIB_2027\\".to_string(), stick_root: "F:\\".into(), ..Config::default() };
        base.apply_library();
        assert_eq!(base.project_lib, "E:\\x\\LIB_2027\\CymaticProjectLib");
        assert_eq!(base.audio_lib, "E:\\x\\LIB_2027\\AudioLibrary");
        assert_eq!(base.import_lib, base.audio_lib);
        save_config(&path, &base).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("audio_lib"));
        let back = load_config(&path);
        assert_eq!(back.audio_lib, "E:\\x\\LIB_2027\\AudioLibrary");
        assert_eq!(back.project_lib, "E:\\x\\LIB_2027\\CymaticProjectLib");
        assert_eq!(back.stick_root, "F:\\");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_mix_presets() {
        let dir = temp_dir("config-mix");
        let path = dir.join("lp16-tool.toml");
        // an old file without presets: one neutral "Default"
        fs::write(&path, "library = \"E:\\x\"\r\nstick_root = \"F:\\\"\r\n").unwrap();
        let old = load_config(&path);
        assert_eq!(old.mixes, mix::MixLibrary::default());
        assert_eq!(old.mixes.active_name(), "Default");

        // three presets, umlauts, spaces, off, solo, active one
        let mut c = Config {
            library: "E:\\x\\LIB".into(),
            stick_root: "F:\\".into(),
            audio_host: "WASAPI".into(),
            audio_device: "Lautsprecher (Realtek)".into(),
            audio_out_left: 3,
            audio_out_right: 4,
            audio_buffer: 512,
            ..Config::default()
        };
        c.mixes.rename_active("B\u{fc}hne laut").unwrap();
        c.mixes.active_mut().set_level(4, f32::NEG_INFINITY);
        c.mixes.active_mut().set_mute(4, true);
        c.mixes.new_mix();
        c.mixes.active_mut().set_solo(7, true);
        c.mixes.active_mut().set_pan(7, -50);
        c.mixes.active_mut().set_master(-4.5);
        c.mixes.new_mix();
        c.mixes.rename_active("Zur\u{fc}ck zum Start").unwrap();
        c.mixes.select("Mix 1").unwrap();
        save_config(&path, &c).unwrap();
        let b = load_config(&path);
        assert_eq!(b.mixes, c.mixes);
        assert_eq!(b.mixes.presets().len(), 3);
        assert_eq!(b.mixes.active_name(), "Mix 1");
        // paths and audio settings are unchanged
        assert_eq!((b.library.as_str(), b.stick_root.as_str()), ("E:\\x\\LIB", "F:\\"));
        assert_eq!(b.audio_device, "Lautsprecher (Realtek)");
        assert_eq!((b.audio_host.as_str(), b.audio_out_left, b.audio_out_right, b.audio_buffer), ("WASAPI", 3, 4, 512));

        // broken lines are ignored
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("mix.1.ch3 = \"kaputt\"\r\nmix.9\r\nmix.1.ch40 = \"0.0 0 0 0\"\r\n");
        fs::write(&path, text).unwrap();
        assert_eq!(load_config(&path).mixes, c.mixes);

        // save_mixes only replaces the mix; the rest of the file stays
        fs::write(&path, "library = \"E:\\y\"\r\nstick_root = \"G:\\\"\r\n").unwrap();
        save_mixes(&path, &c.mixes).unwrap();
        let s = load_config(&path);
        assert_eq!((s.library.as_str(), s.stick_root.as_str()), ("E:\\y", "G:\\"));
        assert_eq!(s.mixes, c.mixes);

        // the backup settings: saved with the rest, save_backup only replaces them
        let mut backup = backup_mix::BackupSettings { dir: "D:\\MP3".into(), pattern: "{nr}_{song}".into(), ..Default::default() };
        backup.mix.right.set_level(15, -2.0);
        save_backup(&path, &backup).unwrap();
        let s = load_config(&path);
        assert_eq!(s.backup, backup);
        assert_eq!((s.library.as_str(), s.stick_root.as_str()), ("E:\\y", "G:\\"));
        assert_eq!(s.mixes, c.mixes, "the rehearsal mix stays");
        save_mixes(&path, &c.mixes).unwrap();
        assert_eq!(load_config(&path).backup, backup, "and the other way round");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_audio_fields() {
        let dir = temp_dir("config-audio");
        let path = dir.join("lp16-tool.toml");
        // an old file without the fields: defaults
        fs::write(&path, "library = \"E:\\x\"\r\nstick_root = \"F:\\\"\r\n").unwrap();
        let old = load_config(&path);
        assert_eq!((old.audio_host.as_str(), old.audio_device.as_str()), ("", ""));
        assert_eq!((old.audio_out_left, old.audio_out_right, old.audio_buffer), (1, 2, 0));
        assert_eq!(old.midi_port, "", "an old config without the field: MIDI is off");
        assert!(!old.rehearsal_autoplay, "an old config without the field: autoplay is off");
        assert_eq!(old.set_pause, 60, "an old config without the field: 60 s pause");
        assert_eq!(old.stick_root, "F:\\");

        // roundtrip of all fields, together with both path styles
        for library in ["E:\\x\\LIB", ""] {
            let own = library.is_empty();
            let c = Config {
                library: library.to_string(),
                audio_lib: if own { "A:\\audio".into() } else { String::new() },
                project_lib: if own { "P:\\prj".into() } else { String::new() },
                import_lib: if own { "I:\\imp".into() } else { String::new() },
                stick_root: "F:\\".into(),
                audio_host: "WASAPI".into(),
                audio_device: "Speakers (Realtek(R) Audio)".into(),
                audio_out_left: 4,
                audio_out_right: 3,
                audio_buffer: 256,
                midi_port: "MIDI \u{fc}ber USB 2".into(),
                rehearsal_autoplay: true,
                set_pause: 45,
                mixes: mix::MixLibrary::default(),
                backup: backup_mix::BackupSettings::default(),
            };
            save_config(&path, &c).unwrap();
            let text = fs::read_to_string(&path).unwrap();
            assert_eq!(text.contains("library ="), !own);
            assert_eq!(text.contains("audio_lib ="), own);
            let b = load_config(&path);
            assert_eq!((b.audio_host.as_str(), b.audio_device.as_str()), ("WASAPI", "Speakers (Realtek(R) Audio)"));
            assert_eq!((b.audio_out_left, b.audio_out_right, b.audio_buffer), (4, 3, 256));
            assert_eq!(b.midi_port, "MIDI \u{fc}ber USB 2");
            assert!(text.contains("midi_port = \"MIDI \u{fc}ber USB 2\""));
            assert!(b.rehearsal_autoplay && text.contains("rehearsal_autoplay = \"true\""));
            assert_eq!(b.set_pause, 45);
            assert_eq!(b.stick_root, "F:\\");
            assert_eq!(b.audio_lib, if own { "A:\\audio" } else { "E:\\x\\LIB\\AudioLibrary" });
            assert_eq!(b.project_lib, if own { "P:\\prj" } else { "E:\\x\\LIB\\CymaticProjectLib" });
        }

        // invalid values give the defaults, valid neighbours stay
        fs::write(
            &path,
            "audio_buffer = \"333\"\r\naudio_out_left = \"x\"\r\naudio_out_right = \"0\"\r\naudio_host = \"ASIO\"\r\n",
        )
        .unwrap();
        let bad = load_config(&path);
        assert_eq!((bad.audio_out_left, bad.audio_out_right, bad.audio_buffer), (1, 2, 0));
        assert_eq!(bad.audio_host, "ASIO");
        fs::write(&path, "audio_out_left = \"999\"\r\naudio_buffer = \"-64\"\r\naudio_out_right = \"18\"\r\n").unwrap();
        let bad = load_config(&path);
        assert_eq!((bad.audio_out_left, bad.audio_out_right, bad.audio_buffer), (1, 18, 0));
        let _ = fs::remove_dir_all(&dir);
    }

    pub(crate) fn mono_wav(frames: usize, rate: u32, bits: u16, channels: u16) -> Vec<u8> {
        let len = frames * (bits as usize / 8) * channels as usize;
        riff(&[chunk(b"fmt ", &fmt_body(channels, rate, bits, 0)), chunk(b"data", &vec![0u8; len])])
    }

    /// minimal SMF, format 0, one track with only "end of track"
    pub(crate) fn tiny_midi() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"MThd");
        b.extend_from_slice(&6u32.to_be_bytes());
        b.extend_from_slice(&[0, 0, 0, 1, 0x01, 0xe0]);
        b.extend_from_slice(b"MTrk");
        b.extend_from_slice(&4u32.to_be_bytes());
        b.extend_from_slice(&[0x00, 0xff, 0x2f, 0x00]);
        b
    }

    #[test]
    fn import_scan_and_write() {
        let root = temp_dir("import");
        let lib = root.join("AudioLibrary");
        // a fresh project library: the import has to create this folder
        let songs = root.join("ProjectLib").join("LP16").join("Multitrack Songs");
        let mk = |rel: &str, bytes: &[u8]| {
            let p = lib.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, bytes).unwrap();
        };
        let w = mono_wav(4800, 48000, 24, 1);
        // new: 3 tracks, natural order -1-, -2-, -10-
        mk("Abc\\2026-05-10_RF\\x-10-cue.wav", &mono_wav(9600, 48000, 24, 1));
        mk("Abc\\2026-05-10_RF\\x-2-bass.wav", &w);
        mk("Abc\\2026-05-10_RF\\x-1-klick.wav", &w);
        mk("Abc\\2026-05-10_RF\\abc.mid", &tiny_midi());
        // a backup of the fade-out repair is no track (stereo would be invalid)
        mk("Abc\\2026-05-10_RF\\x-2-bass.orig.wav", &mono_wav(100, 48000, 24, 2));
        // present by name
        mk("Abc\\2026-01-01\\a.wav", &w);
        // present by WAV folder
        mk("Abc\\2026-02-02\\a.wav", &w);
        // invalid: stereo, two MIDI files
        mk("Def\\2026-03-03\\a.wav", &mono_wav(100, 48000, 24, 2));
        mk("Def\\2026-03-03\\a.mid", &tiny_midi());
        mk("Def\\2026-03-03\\b.mid", &tiny_midi());
        // no date folder: skipped
        mk("Abc\\Audio\\x.wav", &w);
        // a song not in the library: only the newest date folder is new
        mk("Ghi\\2026-01-01\\a.wav", &w);
        mk("Ghi\\2026-02-01_v2\\a.wav", &w);
        let other = lib.join("Abc\\2026-02-02").to_string_lossy().to_lowercase();
        let library = vec![
            LibrarySong { name: "abc_(2026-01-01)".into(), ..Default::default() },
            LibrarySong { name: "Something else".into(), wav_dirs: vec![other], ..Default::default() },
        ];
        let items = scan_import(&lib.to_string_lossy(), &library).unwrap();
        let st: Vec<(&str, &ImportStatus)> = items.iter().map(|i| (i.name.as_str(), &i.status)).collect();
        assert_eq!(st.len(), 6);
        assert_eq!(st[0], ("Abc_(2026-01-01)", &ImportStatus::Present("abc_(2026-01-01)".into())));
        assert_eq!(st[1], ("Abc_(2026-02-02)", &ImportStatus::Present("Something else".into())));
        assert_eq!(st[2], ("Abc_(2026-05-10_RF)", &ImportStatus::Missing));
        assert!(matches!(st[3].1, ImportStatus::Invalid(m) if m.contains("not mono") && m.contains("2 MIDI")));
        assert_eq!(items[1].notes, vec!["no MIDI file".to_string()]);
        assert!(items[2].is_new());
        assert_eq!(items[4].older_than.as_deref(), Some("Ghi_(2026-02-01_v2)"));
        assert!(!items[4].is_new() && items[5].is_new());
        // a newer version of "Abc" in the library (other folder name, same key):
        // 2026-05-10_RF becomes an old folder and is not proposed any more
        let mut lib_newer = library.clone();
        lib_newer.push(LibrarySong {
            name: "_ABC_(2026-06-01)".into(),
            key: Some("abc".into()),
            version: "2026-06-01".into(),
            ..Default::default()
        });
        let old = scan_import(&lib.to_string_lossy(), &lib_newer).unwrap();
        assert_eq!(old[2].older_than.as_deref(), Some("_ABC_(2026-06-01)"));
        assert!(!old[2].is_new());

        let new = &items[2];
        let names: Vec<String> = new.wavs.iter().map(|w| Path::new(w).file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["x-1-klick.wav", "x-2-bass.wav", "x-10-cue.wav"]);
        assert_eq!((new.rate, new.bits), (48000, 24));
        assert!((new.duration - 0.2).abs() < 1e-9);

        // the import lib inside the audio lib -> {$AudioLib} paths
        let audio_lib = root.to_string_lossy().into_owned();
        let written = import_songs(&[new], &songs, &audio_lib).unwrap();
        let xml = fs::read_to_string(&written[0]).unwrap();
        assert_eq!(tag(&xml, "song_name").unwrap(), "Abc_(2026-05-10_RF)");
        assert_eq!(tag(&xml, "track_count").unwrap(), "3");
        assert_eq!(tag(&xml, "duration_seconds").unwrap(), "0.200");
        assert_eq!(tag(&xml, "midi_file").unwrap(), "{$AudioLib}\\AudioLibrary\\Abc\\2026-05-10_RF\\abc.mid");
        let t = project_tracks(&xml);
        assert_eq!(t[2].channel, 2);
        assert_eq!(t[2].audiofile, "{$AudioLib}\\AudioLibrary\\Abc\\2026-05-10_RF\\x-10-cue.wav");
        assert_eq!(t[0].sr, 2);
        // a second import is refused, the file stays
        assert!(import_songs(&[new], &songs, &audio_lib).is_err());
        // after the import the folder counts as present
        let again = scan_import(&lib.to_string_lossy(), &library_songs(&songs, &audio_lib)).unwrap();
        assert_eq!(again[2].status, ImportStatus::Present("Abc_(2026-05-10_RF)".into()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn init_folders_creates_layout() {
        let root = temp_dir("init");
        let config = Config {
            project_lib: root.join("CymaticProjectLib").to_string_lossy().into_owned(),
            audio_lib: root.join("AKTIVE-SONGS").to_string_lossy().into_owned(),
            import_lib: root.join("AKTIVE-SONGS").to_string_lossy().into_owned(),
            ..Config::default()
        };
        let created = init_folders(&config).unwrap();
        assert_eq!(created.len(), 6); // 5 library folders + the (shared) audio folder
        assert!(root.join("CymaticProjectLib").join("LP16").join("Multitrack Songs").is_dir());
        assert!(root.join("CymaticProjectLib").join("Stereo Songs").is_dir());
        assert!(init_folders(&config).unwrap().is_empty()); // second run: nothing to do
        assert!(init_folders(&Config::default()).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn newest_versions() {
        assert_eq!(song_key("_ALL-THE-THINGS-SHE-SAID"), song_key("AllTheThingsSheSaid"));
        assert!(is_date_folder("2026-05-10_RF") && !is_date_folder("transp+2") && !is_date_folder("01_Intro"));
        assert_eq!(
            song_version_of(r"E:\AUDIO\_ABCDEFU\2026-05-10_RF\a.wav"),
            Some(("abcdefu".into(), "2026-05-10_RF".into()))
        );
        // announcement: WAVs not in a date folder -> no version
        assert_eq!(song_version_of(r"E:\AUDIO\auf Social Media hinweisen\a.wav"), None);
        let s = |name: &str, wav: &str| {
            let (key, version) = song_version_of(wav).map(|(k, v)| (Some(k), v)).unwrap_or((None, String::new()));
            LibrarySong { name: name.into(), key, version, ..Default::default() }
        };
        let lib = vec![
            s("_ABCDEFU_(2026-05-10_RF)", r"E:\AUDIO\_ABCDEFU\2026-05-10_RF\a.wav"),
            s("_ABCDEFU_(2025-04-26_RF)", r"E:\AUDIO\_ABCDEFU\2025-04-26_nurCUE\a.wav"),
            s("Abcdefu_copy", r"E:\AKTIVE\Abcdefu\2026-11-20\a.wav"),
            s("Abcdefu_(2026-11-20)", r"E:\AKTIVE\Abcdefu\2026-11-20\a.wav"),
            s("Abcdefu_(2026-11-20_v2)", r"E:\AKTIVE\Abcdefu\2026-11-20_v2\a.wav"),
            s("Ansage", r"E:\AUDIO\auf Social Media hinweisen\a.wav"),
            s("_APT_(2026-05-10_RF)", r"E:\AUDIO\_APT\2026-05-10_RF\a.wav"),
        ];
        let newest = |i: usize| newest_version(&lib[i], &lib).map(|x| x.name.as_str());
        assert_eq!(newest(0), Some("Abcdefu_(2026-11-20_v2)")); // across AUDIO -> AKTIVE-SONGS
        assert_eq!(newest(1), Some("Abcdefu_(2026-11-20_v2)"));
        assert_eq!(newest(4), None); // already the newest
        assert_eq!(newest(5), None); // announcement stays
        assert_eq!(newest(6), None); // no newer APT
        // same date folder twice: the canonical name <Song>_(<Date>) wins
        let lib2: Vec<LibrarySong> = lib.iter().filter(|x| !x.name.ends_with("_v2)")).cloned().collect();
        assert_eq!(newest_version(&lib2[0], &lib2).map(|x| x.name.as_str()), Some("Abcdefu_(2026-11-20)"));
    }

    // -----------------------------------------------------------------------
    // MIDI event reader
    // -----------------------------------------------------------------------

    /// The implementation of midi_duration from before the event reader, kept
    /// unchanged as the reference ("oracle") for the new one.
    fn midi_duration_oracle(src: &str) -> Option<f64> {
        let b = fs::read(src).ok()?;
        if b.len() < 14 || &b[0..4] != b"MThd" {
            return None;
        }
        let be32 = |p: usize| -> Option<u32> {
            b.get(p..p + 4).map(|x| u32::from_be_bytes([x[0], x[1], x[2], x[3]]))
        };
        let hlen = be32(4)? as usize;
        let ntrks = u16::from_be_bytes([b[10], b[11]]) as usize;
        let division = u16::from_be_bytes([b[12], b[13]]);
        let mut tempos: Vec<(u64, u32)> = Vec::new();
        let mut end_tick = 0u64;
        let mut pos = 8 + hlen;
        for _ in 0..ntrks {
            if b.get(pos..pos + 4)? != b"MTrk" {
                return None;
            }
            let len = be32(pos + 4)? as usize;
            let mut p = pos + 8;
            let end = (p + len).min(b.len());
            let mut tick = 0u64;
            let mut running = 0u8;
            let varlen = |p: &mut usize| -> Option<u64> {
                let mut v = 0u64;
                for _ in 0..4 {
                    let c = *b.get(*p)?;
                    *p += 1;
                    v = (v << 7) | (c & 0x7f) as u64;
                    if c & 0x80 == 0 {
                        return Some(v);
                    }
                }
                Some(v)
            };
            while p < end {
                tick += varlen(&mut p)?;
                let mut status = *b.get(p)?;
                if status & 0x80 != 0 {
                    p += 1;
                } else {
                    status = running;
                }
                match status {
                    0xff => {
                        let kind = *b.get(p)?;
                        p += 1;
                        let l = varlen(&mut p)? as usize;
                        if kind == 0x51 && l == 3 {
                            let t = b.get(p..p + 3)?;
                            tempos.push((tick, u32::from_be_bytes([0, t[0], t[1], t[2]])));
                        }
                        p += l;
                        if kind == 0x2f {
                            break;
                        }
                    }
                    0xf0 | 0xf7 => {
                        let l = varlen(&mut p)? as usize;
                        p += l;
                    }
                    _ => {
                        running = status;
                        p += match status & 0xf0 {
                            0xc0 | 0xd0 => 1,
                            _ => 2,
                        };
                    }
                }
                end_tick = end_tick.max(tick);
            }
            pos += 8 + len;
        }
        if division & 0x8000 != 0 {
            // SMPTE time: frames per second * ticks per frame
            let fps = -((division >> 8) as i8) as f64;
            let tpf = (division & 0xff) as f64;
            return Some(end_tick as f64 / (fps * tpf));
        }
        let ppq = division.max(1) as f64;
        tempos.sort_by_key(|t| t.0);
        let (mut sec, mut last_tick, mut tempo) = (0.0f64, 0u64, 500_000u32);
        for (t, us) in tempos {
            if t >= end_tick {
                break;
            }
            sec += (t - last_tick) as f64 * tempo as f64 / 1e6 / ppq;
            last_tick = t;
            tempo = us;
        }
        sec += (end_tick - last_tick) as f64 * tempo as f64 / 1e6 / ppq;
        Some(sec)
    }

    fn vl(mut n: u32) -> Vec<u8> {
        let mut v = vec![(n & 0x7f) as u8];
        n >>= 7;
        while n > 0 {
            v.push((n & 0x7f) as u8 | 0x80);
            n >>= 7;
        }
        v.reverse();
        v
    }

    /// Track body from (delta ticks, event bytes) pairs.
    pub(crate) fn trk(events: &[(u32, &[u8])]) -> Vec<u8> {
        let mut b = Vec::new();
        for (d, e) in events {
            b.extend(vl(*d));
            b.extend_from_slice(e);
        }
        b
    }

    pub(crate) const EOT: &[u8] = &[0xff, 0x2f, 0x00];

    pub(crate) fn smf(format: u16, division: u16, tracks: &[Vec<u8>]) -> Vec<u8> {
        let mut b = b"MThd".to_vec();
        b.extend_from_slice(&6u32.to_be_bytes());
        b.extend_from_slice(&format.to_be_bytes());
        b.extend_from_slice(&(tracks.len() as u16).to_be_bytes());
        b.extend_from_slice(&division.to_be_bytes());
        for t in tracks {
            b.extend_from_slice(b"MTrk");
            b.extend_from_slice(&(t.len() as u32).to_be_bytes());
            b.extend_from_slice(t);
        }
        b
    }

    pub(crate) fn write_mid(dir: &Path, name: &str, bytes: &[u8]) -> String {
        let p = dir.join(name);
        fs::write(&p, bytes).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn midi_reader_format1_events() {
        // 480 ticks per quarter; 120 BPM, from tick 960 on 240 BPM
        let tempo_trk = trk(&[
            (0, &[0xff, 0x51, 0x03, 0x07, 0xa1, 0x20]),
            (960, &[0xff, 0x51, 0x03, 0x03, 0xd0, 0x90]),
            (0, EOT),
        ]);
        let t1 = trk(&[
            (0, &[0xc0, 0x05]),
            (480, &[0xb0, 0x07, 0x64]),
            (0, &[0x07, 0x65]), // running status
            (480, &[0x90, 0x3c, 0x64]),
            (480, &[0xf0, 0x03, 0x7d, 0x01, 0xf7]),
            (0, &[0xf7, 0x01, 0xf8]),
            (0, &[0xff, 0x01, 0x01, b'x']), // meta: not an event
            (0, EOT),
        ]);
        let t2 = trk(&[
            (960, &[0x91, 0x40, 0x00]),
            (0, &[0x91, 0x40, 0x50]),
            (960, &[0x81, 0x40, 0x00]),
            (0, &[0x92, 0x30, 0x00]),
            (0, EOT),
        ]);
        let m = parse_midi_events(&smf(1, 480, &[tempo_trk, t1, t2])).unwrap();
        let expect: &[(f64, &[u8])] = &[
            (0.0, &[0xc0, 0x05]),
            (0.5, &[0xb0, 0x07, 0x64]),
            (0.5, &[0xb0, 0x07, 0x65]),
            (1.0, &[0x90, 0x3c, 0x64]), // track 1 before track 2 at equal time
            (1.0, &[0x91, 0x40, 0x00]),
            (1.0, &[0x91, 0x40, 0x50]),
            (1.25, &[0xf0, 0x7d, 0x01, 0xf7]),
            (1.25, &[0xf8]),
            (1.5, &[0x81, 0x40, 0x00]),
            (1.5, &[0x92, 0x30, 0x00]),
        ];
        assert_eq!(m.events.len(), expect.len(), "{:?}", m.events);
        for (e, (t, bytes)) in m.events.iter().zip(expect) {
            assert!((e.time - t).abs() < 1e-9, "{:?} vs {}", e, t);
            assert_eq!(&e.bytes[..], *bytes);
        }
        assert!((m.duration - 1.5).abs() < 1e-9);
        // Note On with velocity 0 does not count (channel 2)
        let mut ch = [false; 16];
        ch[0] = true;
        ch[1] = true;
        assert_eq!(m.note_on_channels, ch);
    }

    #[test]
    fn midi_reader_smpte() {
        // 25 fps (0xe7 = -25) x 40 ticks per frame = 1000 ticks per second
        let t = trk(&[(0, &[0x90, 0x3c, 0x40]), (100, &[0x80, 0x3c, 0x00]), (900, &[0xb0, 0x07, 0x01]), (0, EOT)]);
        let m = parse_midi_events(&smf(0, 0xe728, &[t])).unwrap();
        assert_eq!(m.events.len(), 3);
        for (e, t) in m.events.iter().zip([0.0, 0.1, 1.0]) {
            assert!((e.time - t).abs() < 1e-9, "{} vs {}", e.time, t);
        }
        assert!((m.duration - 1.0).abs() < 1e-9);
    }

    #[test]
    fn midi_reader_skips_broken_events_but_keeps_length() {
        // a data byte without running status is not sent, but counts for the length
        let file = smf(0, 96, &[trk(&[(10, &[0x3c, 0x40]), (10, &[0x90, 0x3c, 0x40]), (0, EOT)])]);
        let m = parse_midi_events(&file).unwrap();
        assert_eq!(m.events.len(), 1);
        assert_eq!(m.events[0].bytes, vec![0x90, 0x3c, 0x40]);
        let dir = temp_dir("midi-broken");
        let p = write_mid(&dir, "x.mid", &file);
        assert_eq!(Some(m.duration), midi_duration_oracle(&p));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn midi_duration_matches_oracle_on_special_cases() {
        let dir = temp_dir("midi-oracle");
        let tempo = |us: u32| -> Vec<u8> {
            let t = us.to_be_bytes();
            vec![0xff, 0x51, 0x03, t[1], t[2], t[3]]
        };
        let n1: Vec<u8> = vec![0x90, 60, 0x40];
        let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
        // tempo after the last note (and as the last event)
        cases.push(("tempo_after_last", smf(1, 480, &[trk(&[(0, &n1), (480, &tempo(300_000))])])));
        cases.push(("tempo_after_last_eot", smf(1, 480, &[trk(&[(0, &n1), (480, &tempo(300_000)), (0, EOT)])])));
        // tempo at tick 0, twice
        cases.push(("tempo_tick0", smf(1, 480, &[trk(&[(0, &tempo(400_000)), (0, &tempo(600_000)), (960, &n1), (0, EOT)])])));
        // several tracks with different ends, tempo on a second track
        cases.push((
            "tracks_differ",
            smf(
                1,
                96,
                &[
                    trk(&[(0, &n1), (100, &[0x80, 60, 0]), (0, EOT)]),
                    trk(&[(50, &tempo(250_000)), (1000, &n1), (0, EOT)]),
                    trk(&[(10, &n1), (0, EOT)]),
                ],
            ),
        ));
        // meta event as the last event, no EOT
        cases.push(("meta_last", smf(0, 480, &[trk(&[(0, &n1), (700, &[0xff, 0x01, 0x02, b'a', b'b'])])])));
        // end-of-track after a long delta (EOT does not count)
        cases.push(("eot_late", smf(0, 480, &[trk(&[(0, &n1), (100, &[0x80, 60, 0]), (5000, EOT)])])));
        // sysex and F7 escape last, running status, 5-byte varlen
        cases.push(("sysex_last", smf(0, 480, &[trk(&[(0, &n1), (0, &[0x3d, 0x40]), (300, &[0xf0, 0x02, 0x7d, 0xf7])])])));
        cases.push(("escape_last", smf(0, 480, &[trk(&[(0, &n1), (123, &[0xf7, 0x01, 0xf8])])])));
        cases.push(("varlen5", smf(0, 480, &[vec![0x81, 0x81, 0x81, 0x81, 0x01, 0x90, 60, 64, 0x00, 0xff, 0x2f, 0x00]])));
        // SMPTE (25 and 30 fps)
        cases.push(("smpte25", smf(0, 0xe728, &[trk(&[(0, &n1), (333, &[0x80, 60, 0]), (0, EOT)])])));
        cases.push(("smpte30", smf(1, 0xe210, &[trk(&[(7, &n1), (0, EOT)]), trk(&[(99, &n1), (0, EOT)])])));
        // empty track, no track at all, ppq 0
        cases.push(("empty_track", smf(1, 480, &[trk(&[(0, EOT)]), trk(&[])])));
        cases.push(("no_tracks", smf(1, 480, &[])));
        cases.push(("ppq0", smf(0, 0, &[trk(&[(0, &n1), (100, &[0x80, 60, 0])])])));
        // header problems
        let mut bad = smf(0, 480, &[trk(&[(0, &n1), (0, EOT)])]);
        bad[0] = b'X';
        cases.push(("bad_header", bad));
        let mut not_a_track = smf(0, 480, &[trk(&[(0, &n1), (0, EOT)])]);
        not_a_track[14] = b'X';
        cases.push(("not_a_track", not_a_track));
        cases.push(("short", b"MThd".to_vec()));
        cases.push(("empty", Vec::new()));
        // a file cut off at every position
        let full = smf(
            1,
            480,
            &[
                trk(&[(0, &tempo(500_000)), (960, &tempo(250_000)), (0, EOT)]),
                trk(&[(0, &[0xc0, 5]), (480, &[0xb0, 7, 100]), (0, &[7, 101]), (480, &[0xf0, 3, 0x7d, 1, 0xf7]), (0, EOT)]),
            ],
        );
        for n in 0..=full.len() {
            cases.push(("cut", full[..n].to_vec()));
        }
        assert!(cases.len() >= 8);
        let mut some = 0;
        for (name, bytes) in &cases {
            let p = write_mid(&dir, "case.mid", bytes);
            let (new, old) = (midi_duration(&p), midi_duration_oracle(&p));
            assert_eq!(new, old, "{} ({} bytes)", name, bytes.len());
            // the reader itself agrees (and does not panic)
            assert_eq!(read_midi_events(&p).map(|m| m.duration), old, "{}", name);
            some += new.is_some() as usize;
        }
        assert!(some > 10 && some < cases.len());
        // missing file
        let missing = dir.join("nope.mid").to_string_lossy().into_owned();
        assert_eq!(midi_duration(&missing), None);
        assert_eq!(midi_duration_oracle(&missing), None);
        let _ = fs::remove_dir_all(&dir);
    }

    fn find_mid_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.path());
        for e in entries {
            let p = e.path();
            if p.is_dir() {
                find_mid_files(&p, out);
            } else if p.extension().map_or(false, |x| x.eq_ignore_ascii_case("mid")) {
                out.push(p);
            }
        }
    }

    #[test]
    #[ignore = "needs the reference data (LP16_TEST_PROJECT_LIB, LP16_TEST_AUDIO_LIB, LP16_TEST_STICK; cargo test -- --ignored)"]
    fn midi_duration_matches_oracle_on_reference_library() {
        let mut files = Vec::new();
        find_mid_files(Path::new(&test_env("LP16_TEST_AUDIO_LIB")), &mut files);
        assert!(!files.is_empty(), "no .mid file found under LP16_TEST_AUDIO_LIB");
        for f in &files {
            let s = f.to_string_lossy();
            let (new, old) = (midi_duration(&s), midi_duration_oracle(&s));
            assert_eq!(new, old, "{}", s);
        }
        println!("compared {} .mid files", files.len());
    }
    #[test]
    fn project_lib_with_a_backslash_at_the_end_or_a_drive_root() {
        let song = |p: &str| PlaylistSong { name: "Intro".into(), songprj: p.into(), mode: "m".into(), delay: "0".into() };
        for (lib, scanned) in [
            ("C:\\Lib\\", "C:\\Lib\\LP16\\Multitrack Songs\\Intro.songprj"),
            ("C:\\Lib", "C:\\Lib\\LP16\\Multitrack Songs\\Intro.songprj"),
            ("D:\\", "D:\\LP16\\Multitrack Songs\\Intro.songprj"),
        ] {
            // the path as the folder scan builds it
            assert_eq!(Path::new(lib).join("LP16\\Multitrack Songs").join("Intro.songprj").to_string_lossy(), scanned);
            let xml = generate_playlistprj("Set", 1, lib, &[song(scanned)]);
            assert!(xml.contains("{$ProjectLib}\\LP16\\Multitrack Songs\\Intro.songprj"), "{}", lib);
            // read back: exactly the scanned path, no doubled "\"
            let (_, songs) = parse_playlist(&xml, lib);
            assert_eq!(songs[0].songprj, scanned, "{}", lib);
            assert_eq!(playlist_song_prjs(&xml, lib), vec!["LP16\\Multitrack Songs\\Intro.songprj"]);
        }
        // the library folder on a drive root
        let mut c = Config { library: "D:\\".into(), ..Config::default() };
        c.apply_library();
        assert_eq!(c.project_lib, "D:\\CymaticProjectLib");
        assert_eq!(c.audio_lib, "D:\\AudioLibrary");
    }
}

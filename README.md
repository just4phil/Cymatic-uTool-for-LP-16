# lp16-tool

**English** · [Deutsch](README_german.md)

**A replacement for uTool2: manage songs and playlists for the Cymatic Audio LP-16 and export them to the stick.**

Cymatic Audio no longer exists, and uTool2 is no longer maintained. lp16-tool reads the uTool2
project library (`.songprj`, `.playlistprj`) and writes the stick (`Recording\<Song>\…`,
`Recording\#PLAYLISTS\*.play`) **byte for byte** the way uTool2 does. It comes with a user
interface modelled on uTool2 and a command line.

> **Status:** lp16-tool supports **the LP-16 only** and is **not a complete rebuild** of uTool2.
> What is missing or works differently is listed under
> [What is missing and what is different](#what-is-missing-and-what-is-different). The
> **Import** tab does something entirely different from the original, see
> [Import](#import-batch-import-of-songs).

> lp16-tool is an independent project and is not affiliated with Cymatic Audio. It is written
> from scratch in Rust and contains no code from uTool2. The file formats were derived from
> uTool2's behaviour and from the files on a real stick.

## Features

The **manual** (German) covering all features is a single HTML file,
[`docs/Handbuch.html`](docs/Handbuch.html); it is also part of every release archive.

- **Stick export like uTool2:** `song.set` (including the `validationcode`), `.play` and the WAVs
  (header rebuilt, audio data unchanged, names `_<source>_<NN>.wav`) are byte-identical to what
  uTool2 writes.
- **Quick Export and Force Overwrite:** Quick Export writes only new and changed songs, Force
  Overwrite writes all selected songs again. The preview shows *New Export*, *Overwrite*, *Skip*
  or *Force Overwrite* for each song, plus the amount of data ("To Copy").
- **Safe writing:** every file is first written as `.lp16tmp` and then renamed. After each song
  the result on the target is checked. A cancelled export never leaves a half-written file.
- **Check without writing:** `verify` compares a stick with the library and exits with code 0
  only if everything is up to date. Handy before every gig.
- **Start mode per song:** in the playlist editor, check the songs, choose a start mode ("Wait for
  button press", "Wait for timer" with a delay in seconds, "No wait") and click "Apply to selected
  rows". The LP-16 then waits for Play or starts the song by itself. The values are the ones uTool2
  writes (`wait_for_button_press`, `wait_period`, `no_wait`).
- **Edit the library:** create, reorder, rename, copy and delete playlists; create songs and drag
  WAVs onto the 16 tracks in the song editor, choose the MIDI file.
- **Clean song endings:** detects WAVs that end with digital clipping or stop abruptly (*No clean
  fade-out* in the song details; checked when a song is shown, after every import and for the whole
  library with *Check song endings*) and repairs them on a click with a 50 ms fade-out, optionally
  after cutting off the broken end. The original is kept as `.orig.wav`.
- **Batch import of songs:** creates songs from new folders of the AudioLibrary in one go, see
  [Import](#import-batch-import-of-songs).
- **Edit Target:** the contents of the stick as a tree, with details on songs and playlists.
  Songs and playlists can be deleted on the stick (Del key, context menu or "Delete" button);
  "Delete all songs not in playlists" removes every song folder no playlist on the stick lists.
- **Rehearsal player:** plays songs and playlists of the library on an audio output of your
  choice, with a 16-channel rehearsal mix, see [Rehearsal player](#rehearsal-player).
- **Backup MP3s:** writes a stereo MP3 per song of a playlist (backings left, click right) as an
  emergency backup, see [Export Stereo-MP3](#export-stereo-mp3-a-backup-for-emergencies).

## Requirements

- Windows (developed and tested on Windows 11)
- [Rust](https://www.rust-lang.org/tools/install) (stable), only for building
- A uTool2 project library, or an empty folder that lp16-tool sets up

A ready-built exe is available under [Releases](../../releases). It includes ASIO output; the
ZIP also contains the manual and the license notices.

## Build and run

```
cargo build --release
target\release\lp16-tool.exe
```

Without arguments (also by double-click) the user interface starts. To build without it (and
without the audio and MIDI dependencies `cpal`, `rtrb` and `midir` of the rehearsal player):
`cargo build --release --no-default-features`.

### Building with ASIO

The standard build outputs through WASAPI and needs neither the ASIO SDK nor LLVM. The
**ASIO** output (e.g. for the Behringer XR18 with the "X-AIR ASIO Driver") is the Cargo feature
`asio`. Public releases contain it. To build it yourself (Windows):

1. Install LLVM (its libclang is needed to compile the ASIO bindings):
   `winget install --id LLVM.LLVM -e`
2. Download the ASIO SDK from [steinberg.net/asiosdk](https://www.steinberg.net/asiosdk) (the
   version with the GPLv3 option) and unpack it, e.g. to `C:\SDKs\asiosdk`, so that the folders
   `common`, `host` etc. are directly in it.
3. Set the environment variables (for new shells; `setx` saves them for the user):
   ```
   setx LIBCLANG_PATH "C:\Program Files\LLVM\bin"
   setx CPAL_ASIO_DIR "C:\SDKs\asiosdk"
   ```
4. Build:
   ```
   cargo build --release --features asio
   ```

With the feature, "ASIO" appears in the host list of the Settings tab and the device list shows
the installed ASIO drivers.

## Setup

The settings live in `lp16-tool.toml`. The file is looked for in the current folder, otherwise
next to the exe and in the folders above it. Use
[`lp16-tool.example.toml`](lp16-tool.example.toml) as a template:

```
copy lp16-tool.example.toml lp16-tool.toml
```

`lp16-tool.toml` contains your own paths and is therefore not in the repo (`.gitignore`). If the
file is missing, the user interface creates it next to the exe the first time you change a
setting. The easiest way is to set the **Library Folder** in the **Settings** tab; "Initialize
Folders" creates any missing folders.

```toml
# base folder: <library>\CymaticProjectLib = project library,
#              <library>\AudioLibrary      = audio files and import
library = "E:\LP16_LIBRARY"
# export target (chosen and remembered in the Export tab)
stick_root = "F:\"
# audio output of the rehearsal player (Settings > Audio output, see below)
audio_host = "WASAPI"
audio_device = ""
audio_out_left = "1"
audio_out_right = "2"
audio_buffer = "0"
# MIDI output port of the rehearsal player (Settings > MIDI output); empty = off
midi_port = ""
# language of the user interface (Settings > Language): "en" or "de"; empty = English
language = "en"
# rehearsal mix presets (written by the program, see "Rehearsal mix")
mix.1.name = "Default"
mix.1.master = "0.0"
mix.1.ch1 = "0.0 0 0 0"
mix_active = "Default"
```

Layout of the base folder:

```
<library>\
├── CymaticProjectLib\
│   └── LP16\
│       ├── Multitrack Songs\       *.songprj
│       └── Multitrack Playlists\   *.playlistprj
└── AudioLibrary\
    └── <Song>\<YYYY-MM-DD…>\       *.wav, one .mid
```

Older files with separate paths (`audio_lib`, `project_lib`, `import_lib`) are still read.

Back up `Recording\` before the first export to a real stick.

## Rehearsal player

The player plays a song of the project library on the **audio output** you choose in the
Settings tab (WASAPI or ASIO), for rehearsing at home. It also sends the MIDI file of the song to a
**MIDI output** (see [MIDI output](#midi-output-settings-tab)).

- In the Home tab, right-click a song and choose **Play in Rehearsal**. This loads the song,
  switches to the **Rehearsal** tab and starts playing.
- To play a playlist, right-click it in the Home tab and choose **Play in Rehearsal**, or pick it
  in the **Playlist** combo at the left of the Rehearsal tab. This loads the first song and
  switches to the tab; it does **not** start by itself, because live the first song waits for the
  button, too. The combo also shows "(single song)" while a single song is loaded.
- The tab shows the song name, position and length, a progress bar (click or drag to jump), Play/Pause,
  Stop and "−10 s" / "+10 s" buttons. A counter appears when the audio buffer ran empty (dropouts). Warnings name
  the track and the reason, e.g. a missing or unreadable WAV. The song plays with the other
  tracks anyway.
- Jumping while playing fades out and back in over 5 ms, so nothing clicks. While you drag the
  progress bar the time shows the target; the jump happens when you let go.
- Keyboard shortcuts work while the Rehearsal tab is shown and no text field has the focus:

  | Key | Function |
  |---|---|
  | Space | Play/Pause |
  | Left / Right | −10 s / +10 s |
  | Shift + Left / Right | −30 s / +30 s |
  | Home | jump to 0, the state stays |
  | Esc | Stop |

  | Up / Down | move the mark to the previous / next song (like a click) |

  The same functions are on the buttons next to Play.
- **Playlists:** the song list at the left shows number, name, length, a note symbol if the song
  has a MIDI file and a mixer symbol if it has its own [song mix](#song-mix). A song that cannot be read (e.g. its `.songprj` is missing) is red, with the reason
  as tooltip, and is skipped. **Exactly one song is always marked** (coloured background); after a
  playlist is loaded it is the first playable one. **Play starts the marked song** (a loaded, paused
  song goes on at its position).
  - **Click** a song to mark it. If nothing plays (stopped, paused, ended, waiting, countdown) the song
    is loaded at position 0 and stopped, so Play starts it; a running countdown is cancelled. While a
    song plays it goes on: the clicked song is marked and becomes the **next** song, so it follows when
    the current one ends (a click on the playing song takes that back). The playing song has a ▶ in
    front of its name, the next one an arrow →.
  - **Double-click** starts the song at once.
  - The keys Up and Down move the mark and click, and Space is Play.
  - When a song ends, the mark moves to the next song (the clicked one, if there is one). At the end of
    the playlist it stays on the last song and the state is "ended"; Play then plays that song again.
  - The buttons ⏮ and ⏭ go to the previous and next song:
    - Next: while a song plays the next one starts at once; otherwise it is loaded at position 0 and
      stopped. At the last song nothing happens.
    - Previous: further than 3 s into the song it jumps to its start, otherwise the song before is
      loaded (it starts at once if the player was playing).
- **Autoplay after 5 seconds** (checkbox above the song list, saved as `rehearsal_autoplay` in `lp16-tool.toml`, off by default). Off: when a song ends, the next
  one is marked and loaded, and the player waits (the tab says "Next: <song> – press Play").
  On: the next song starts by itself after 5 seconds of silence (the tab says "Next: <song> in <n> s").
  Play during the countdown starts it at once, Pause cancels it, switching autoplay off ends it.
  The setting replaces the start mode of the playlist: the player no longer looks at `start_mode` and
  `start_mode_delay` in the `.playlistprj` (the file is not changed). A single song has no next song.
- The next song is opened while the current one has less than 10 s left (and while waiting), so it
  starts without a noticeable delay. If it has another sample rate, the output is opened again with
  that rate before it starts.
- If the loaded playlist is saved in the playlist editor, the queue is built again; the song that
  plays goes on at the same position if it is still in the playlist, otherwise the player stops on
  the first song. A renamed playlist stays selected under its new name. If it is deleted, the player
  stops and unloads it, with a message.
- All tracks are streamed from disk in sync (nothing is loaded into memory up front) and mixed
  to stereo by the [rehearsal mix](#rehearsal-mix); the result is limited to full scale.
- The stream is opened with the **sample rate of the song**. If the next song has another rate,
  the stream is closed and opened again with the new rate. There is no resampling; the song is
  only not played if the device does not support its rate (the tab says so).
- WAVs must be mono with 8, 16, 24 or 32 bit integer or 32 bit float samples. A track with
  another sample rate than the first readable track stays silent.
- If a song is renamed, deleted or saved in the song editor while it is in the loaded queue, the
  player stops and unloads it and the tab asks you to load it again.
- Export, import and library editing keep working while a song plays.
- Without a usable audio device the tab shows a message, and Play tries again. If the device
  disappears during playback, playback stops, the position stays and the tab says "Audio device
  lost"; after reconnecting, press Play and the output is opened again, no restart needed.

### Rehearsal mix

Below the player in the Rehearsal tab is the **mixer**: 16 channel strips (LP-16 output 1 to 16)
and a master, in the style of the uTool2 mixer. The song list, the player and the mixer are
panels: drag the line between the list and the player, or between the player and the mixer, to
make them wider or taller. The strips fill the width and height of the mixer (they get wider and
the faders taller with it); below a minimum size the mixer scrolls.

The mixer edits the **rehearsal mix** (it applies to all songs) or, in song mode, the
[song mix](#song-mix) of the loaded song. Each track of the loaded song plays on the strip of its
LP-16 output (the channel in the `.songprj`); several tracks on one output are summed in that
strip. A track with an output beyond 16 stays silent and adds a warning. The strip shows the
output number and the track name; a strip without a track is greyed out ("—") but can still be
used.

- Per strip: **S**olo and **M**ute (a muted strip stays silent even with Solo), a pan knob
  (−127 to +127 like uTool2, constant power law; drag up or down, double-click = centre), the
  level in dB, a fader with a dB scale (−60 to +6 dB, the very bottom is "off", double-click =
  0 dB) and a level meter.
- **Typing a level:** double-click the dB value above the fader and type it, e.g. `-0.5`, `-0,5`,
  `+3`, `-12 dB`, or `-inf` / `off`. Enter or a click elsewhere takes it, Esc cancels.
- The fader is finer in the range that matters: −10 dB to +6 dB take 60 % of its travel
  (0 dB is at 80 %), −60 to −10 dB the rest.
- Master: fader, meter for left and right and a **CLIP** LED. It lights when the master had to
  limit the signal to full scale and stays on until you click it.
- The meters show the peak after fader and mute (song mix included), before pan; they fall by
  20 dB per second.
- Changes act at once during playback without clicks (the gains move over 10 ms).
- **Presets** are named mixes: choose one in the list, **New** (neutral, "Mix N"), **Copy**,
  **Rename**, **Delete** (the last one cannot be deleted) and **Reset** (back to neutral: all
  strips 0 dB, centre, no Mute/Solo, master 0 dB). Names must be unique and not empty, and must
  not contain a double quote or a line break.
- The presets are saved automatically in `lp16-tool.toml` (at most once per second after a
  change, and always when the program ends) as `mix.<n>.name`, `mix.<n>.master`,
  `mix.<n>.ch<k> = "<level> <pan> <mute> <solo>"` (level with one decimal or `off`) and
  `mix_active`.
- **The rehearsal mix is not exported.** It never goes into a `.songprj`, a playlist, a
  `song.set` or a `.play`, and not onto the stick.

### Song mix

The **song mix** is the uTool2 mixer of a song: level and mute per track (`monitor_level`,
`monitor_mute` in the `.songprj`). It is saved in the song and exported to the stick with it
(`output_level`, `channel_mute` in the `song.set`). Use it to correct a single song quickly, e.g. a
backing track that is a little too loud.

- **Open it:** in the Home tab, the **Mixer** button of a song (also in the song editor) loads the
  song into the player (without starting) and shows its song mix in the mixer of the Rehearsal tab.
  Or click **Song mix** next to the preset bar: it edits the song that is loaded. In song mode the
  strips are brown and the header shows "Song mix" and the song; the mode follows the loaded song
  (another song in the list, the next song of the playlist).
- In song mode the fader and the dB value are the song's level (0 dB = no change, shown as
  `+2.0 dB` etc.; −60 to +6 dB, there is no "off": use Mute) and **M** is the song's mute. Solo and
  pan stay those of the rehearsal mix (the knob is greyed out): the LP-16 only pans its headphone
  output. Strips without a track in the song cannot be changed. **Reset** sets all to 0 dB and
  unmuted; **Back to the rehearsal mix** ends song mode.
- **In the rehearsal** the song mix adds to the rehearsal mix, for this song only: level =
  rehearsal level + song level (e.g. rehearsal bass −3 dB, song bass +2 dB: −1 dB), and a track
  muted in the song is silent. It applies from the first sample of the song, also when autoplay
  goes to the next song. Changes act at once while the song plays.
- In the rehearsal mix, a strip on which the loaded song has its own value shows it in orange
  below the track name ("song +2.0", "song: mute").
- Changes are written into the `.songprj` a moment after the last change (and when the song
  changes, song mode ends or the program closes). All tracks on one output get the same values;
  the pan of the song is kept as it is. Saving the song editor keeps the song mix in the file.
- A song whose mixer values differ from uTool2's defaults (0 dB, centre, not muted) has a **mixer
  symbol** (three small faders) in the song list of the Home tab and of the Rehearsal tab.

### Language (Settings tab)

The **Language / Sprache** area at the top of the Settings tab switches the user interface
between English and German. The change applies at once and is saved as `language` in
`lp16-tool.toml`. The texts are in `lang/en.lang` and `lang/de.lang` (compiled into the exe);
the messages of the library (errors, export report) follow the same setting. The command line
stays English.

### Audio output (Settings tab)

The **Audio output** area of the Settings tab chooses where the stereo mix goes. Changes are saved
at once in `lp16-tool.toml` and used the next time the stream is opened (a playing song is not
interrupted). The program starts without a message even if the saved device is missing; the
message comes when you press Play or Test tone.

| Setting | Key in `lp16-tool.toml` | Meaning | Default |
|---|---|---|---|
| Host | `audio_host` | audio system: `WASAPI`, and `ASIO` in a build with the `asio` feature | default host |
| Device | `audio_device` | name of the output device | empty = default device |
| Left / Right | `audio_out_left`, `audio_out_right` | output channels (1-based) for the left and right side of the stereo mix; the same channel twice gets (L+R)/2 | 1 / 2 |
| Buffer | `audio_buffer` | frames: 0 = driver default, 64, 128, 256, 512, 1024, 2048 | 0 |

- **Refresh** reads the list of devices again (e.g. after plugging in a device).
- **Test tone** plays a 1 kHz tone at -20 dBFS, 1 s on the left channel, then 1 s on the right
  channel, with the settings above (48 kHz, or the default rate of the device if it cannot do
  that). The button is disabled while a song plays.
- While a stream runs, the area shows "Running: <rate> Hz, buffer <N> frames". If the device
  rejects the chosen buffer size, the driver default is used and a note says so.
- Invalid values in the file (e.g. `audio_buffer = "333"`) are replaced by the default.

**ASIO** (build with `--features asio`): the device list shows the installed ASIO drivers, and
Left / Right choose among the output channels of the driver (18 for the XR18). The song's sample
rate is requested from the driver; if it refuses, a message says so. If the driver does not take
the buffer size, its own setting is used and the note says to set the buffer size in the ASIO
driver's control panel. Many ASIO drivers serve only one application: if your DAW uses the
driver, opening it fails with a message that suggests this. The driver's sample formats `i16`,
`i32`, `f32` and `f64` are all supported.

**Behringer XR18:** put the stereo mix on two USB channels (e.g. 17 and 18, with the X-AIR
driver the USB return channels) and in X Air Edit route a channel or an aux return with source
USB to these channels.

### MIDI output (Settings tab)

The **MIDI output** area of the Settings tab chooses the MIDI port the MIDI file of the song is
sent to. The choice is saved at once as `midi_port` in `lp16-tool.toml` (empty = no MIDI output,
the default; an old file without the key also means off).

- The combo lists **(none)** and all MIDI output ports of the system (on Windows there is always
  "Microsoft GS Wavetable Synth"); **Refresh** reads the list again.
- **Send test** sends the state the loaded song would give a device right after its start: bank
  select, program change and the controllers that lie on the very start of the song. The modeler
  only gets what it would get at the start of the song anyway. The button needs a port and a
  loaded song with a MIDI file.
- The port is opened with Play (or with the test), not at program start. If the saved port is
  missing or cannot be opened, the Rehearsal tab and the settings say "MIDI port not available:
  <name>", the audio plays anyway, and the next Play tries again.
- The Rehearsal tab shows "MIDI: <port> · <n> events" under the audio output, with a dot that
  lights up for 150 ms at every message sent. "MIDI: no file" means the song has no (readable) MIDI
  file, "MIDI: off" that no port is chosen. The song list shows a MIDI symbol for songs that have a
  file.

**How the MIDI is sent.** The MIDI file is sent in time with the audio, tempo changes included.
The band's files contain no notes and no clock, only bank select (CC32) and program change on
channel 1 at the start, CC69 (snapshot selection on Line 6 devices) with the values 1 to 5
spread over the song, and CC22 on channel 10. The audio callback never sends MIDI itself: it
queues the messages with a target time (time of the callback + output latency of the stream +
position in the block), and a MIDI thread sends them on time.

**Chasing.** When the song starts at a position after the beginning, and at every jump while it
plays, the MIDI state at that position is sent first, at the moment the audio comes back: for
each MIDI channel the bank select (CC0, CC32), the last program change and the last value of
every other controller (1 to 119). Notes, pitch bend, aftertouch, SysEx and channel mode
messages are not chased. An event exactly on the target position is sent as a normal event.
Because of this, **CC69 and the program change give the right snapshot after a jump**. Starting
at 0 and continuing after a pause send each event exactly once, with no chasing.

**All Notes Off** (CC123) goes to every channel on which the file has notes when you stop or
pause, jump, change the song or close the program. Files without notes send nothing.

**Behringer XR18:** connect the XR18 over USB and choose its MIDI port here. The XR18 passes the
messages on from its MIDI out to the modeler.

## Export Stereo-MP3: a backup for emergencies

If the LP-16 fails on stage, the show goes on from a phone or an MP3 player: the **Export
Stereo-MP3** tab writes one stereo MP3 per song of a playlist whose two sides are two separate
mono mixes of the 16 LP-16 outputs – typically **the backing tracks on the left** (synths,
samples, ...) and **the click and cues on the right**. The audience gets the backings over the PA,
the drummer gets the click in the headphones.

- **Two mixers:** "Left side" and "Right side" each have 16 strips (level, mute) and a master.
  Everything starts *off*; whatever should go to a side is turned up. A channel can be on both
  sides. Strips that reach a side are coloured, and the line above the mixers shows which
  outputs end up left, right, on both or nowhere. The strip names come from the songs of the
  chosen playlist and wrap, so they can be read in full; a double-click on a name renames the
  channel (for both sides, saved; empty = the name from the songs again). The mix is global (for all songs).
- **Preview:** the "Preview" bar at the top is the rehearsal player: choose a song, Play/Pause,
  Stop, ±10 s, previous/next song, and click or drag in the timeline to jump. While the tab is
  shown the player plays the backup mix – exactly what goes into the MP3. "L + R", "Left only"
  and "Right only" play both sides as in the MP3, or one side on both speakers. The strips show
  levels, the CLIP light comes on when a side would clip. In the Rehearsal tab the rehearsal mix
  applies again.
- **Song mix:** the uTool2 mixer values of a song (see [Song mix](#song-mix)) work as in the
  rehearsal player: level of the backup mixer + level of the song, a muted track stays silent.
- **Format:** MP3, 192 kbit/s CBR, LAME quality 0 (the best), plain stereo (not joint stereo, so
  nothing of the click can leak into the backing side), sample rate as the song (44.1 or 48 kHz).
  The MIDI file of a song is not part of the MP3.
- **File names:** the pattern knows `{nr}` (position in the playlist, at least two digits: 01,
  02, ...), `{song}` and `{playlist}`; the default is `{nr} {song}`. Characters Windows does not
  allow become `_`, duplicate names get ` (2)`. A preview shows the first file name. Existing
  files of the same name are overwritten.
- **ID3 tags:** title = song name, album = playlist name, track = position/count.
- **Overload:** if a side goes above 0 dBFS it is clipped; the report names the peak level and
  the number of clipped samples. Lower the master of that side and export again.
- **Report:** per song the duration, errors and notes (tracks left out, MIDI, clipping). Cancel
  works at any time; no half-written MP3 is left behind.

Mixers, target folder and pattern are stored in `lp16-tool.toml` (`backup_dir`,
`backup_pattern`, `backup.left.*`, `backup.right.*`). A 5-minute song with 16 tracks takes about
2 seconds.

## Command line

```
lp16-tool song     <songprj>     <audio-lib> <out-dir>
lp16-tool playlist <playlistprj> <project-lib> <audio-lib> <out-dir>
lp16-tool export   <playlistprj> <project-lib> <audio-lib> <stick-dir> [--dry-run] [--force]
lp16-tool verify   <playlistprj> <project-lib> <audio-lib> <stick-dir>
lp16-tool gui
```

| Command | What it does |
|---|---|
| `song` | writes `<out-dir>\<Song>\song.set` and prints the `validationcode` |
| `playlist` | writes `<out-dir>\#PLAYLISTS\<Playlist>.play` |
| `export` | puts a playlist with all its songs onto the stick (`<stick>\Recording\…`); `--dry-run` only shows what would happen, `--force` = Force Overwrite |
| `verify` | checks the stick read-only; exit code 0 = up to date |

- `audio-lib` replaces `{$AudioLib}` in the `.songprj` (in uTool2: `AudioFileLibraryPath` in
  `%APPDATA%\Cymatic Audio uTool2\Cymatic Audio uTool2.settings`).
- `project-lib` is the root of the project library and replaces `{$ProjectLib}` in the
  `.playlistprj`.

Example:

```
lp16-tool verify ^
  "E:\LP16_LIBRARY\CymaticProjectLib\LP16\Multitrack Playlists\Set 2026.playlistprj" ^
  "E:\LP16_LIBRARY\CymaticProjectLib" ^
  "E:\LP16_LIBRARY\AudioLibrary" ^
  "F:\"
```

A `.songprj` that cannot be read stops `export` before anything is written.

## Import: batch import of songs

The **Import** tab shares only its name with uTool2's import.

- **uTool2** imports audio from a folder or an external drive into the project library, with
  options such as "Split at Markers".
- **lp16-tool** creates songs in a **batch**. It expects an AudioLibrary with one folder per song
  and, inside it, one folder per version, named with a date:

  ```
  AudioLibrary\<Song>\<YYYY-MM-DD…>\   up to 16 mono WAVs, at most one .mid
  ```

  "Import Songs" scans the AudioLibrary and suggests every version folder that has no song yet
  and is newer than the newest version in the library. After confirming, one `.songprj` is
  created per folder, with the WAVs on tracks 1…n (natural sort order) and the MIDI file.
  Faulty folders (not mono, mixed formats, more than 16 WAVs, several MIDI files) are shown but
  not imported. Existing files are never overwritten.

**Update Playlist** belongs to this: it builds a new playlist from an existing one, with every
song replaced by its newest version. After an import, this is suggested automatically.

## When a song is exported again

Quick Export writes a song if

- its folder is missing on the stick (*New Export*),
- a WAV is missing or no longer has the size the current source produces,
- the MIDI file is missing, or
- the `song.set` differs. The `validationcode` in it contains the paths and modification times
  of the source files, so re-rendered WAVs of the same length and changed MIDI files are caught
  as well.

Otherwise it is skipped (*Skip*). The `.play` of a selected playlist is written on every export.
Force Overwrite writes all selected songs again; this helps if files on the stick might be
damaged. Nothing is deleted on the stick.

## Code structure

| File | Contents |
|---|---|
| `src/lib.rs` | File formats: `SongProject` (one `.songprj`, producing `song.set` and the `validationcode`), `.play`, `.playlistprj`, reading WAV and MIDI, import, settings |
| `src/project_lib.rs` | Project Library: list, create, rename, copy and delete songs and playlists. Renaming a song keeps the playlists pointing at it; a song that a playlist uses cannot be deleted. |
| `src/wav_tail.rs` | Checks the end of a WAV (clipping, abrupt stop) and repairs it with a fade-out, keeping a `.orig.wav` backup |
| `src/sync.rs` | Stick Sync: `plan` (what an export would do), `run` (export and verify). Command line and user interface use the same process. |
| `src/mix.rs` | The rehearsal mix: strips, presets, config lines, saving timer |
| `src/backup_mix.rs` | The backup mix of the stereo MP3s: left and right side, config lines, file name pattern |
| `src/backup.rs` | Renders a song into the backup MP3 (mixing, clipping, LAME, ID3 tag) (feature `mp3`) |
| `src/rehearsal.rs` | Playback engine of the rehearsal player (reading, mixing, meters, transport, playlist queue with marking, autoplay and preloading), independent of any audio device (feature `player`) |
| `src/rehearsal_queue.rs` | The queue of the player: entries, the mailbox that opens songs for the engine, rows of the song list (feature `player`) |
| `src/rehearsal_midi.rs` | MIDI of the engine: events as frames, the messages of a block, chasing (feature `player`) |
| `src/midi_out.rs` | MIDI output adapter (`midir`): ports, the MIDI thread with target times, All Notes Off at the end (feature `player`) |
| `src/audio_out.rs` | Audio output adapter: hosts, devices and streams via `cpal`, channel distribution, test tone, commands and status between the user interface and the audio callback (feature `player`) |
| `src/main.rs` | Command line |
| `src/gui_app.rs` | User interface (egui 0.27) |
| `src/gui_rehearsal.rs` | The Rehearsal tab |
| `src/gui_tail.rs` | Background check of the song endings (button in Home, after an import) |
| `src/gui_playlist.rs` | Playlist combo and song list in the Rehearsal tab |
| `src/gui_mixer.rs` | Preset bar, meters, S/M buttons and the mixer symbol of the mixer |
| `src/gui_mixer_tab.rs` | The mixer of the Rehearsal tab: uTool2 style strips that scale with the panel, rehearsal mix and song mode |
| `src/gui_audio.rs` | The "Audio output" area of the Settings tab |
| `src/gui_midi.rs` | The "MIDI output" area of the Settings tab |
| `src/i18n.rs` | Translations: `tr!("key")` looks up the texts of `lang/*.lang` in the chosen language |
| `src/gui_backup.rs` | The Export Stereo-MP3 tab |

There are no dependencies apart from `eframe` and `rfd` for the user interface and `cpal` and
`rtrb` and `midir` for the rehearsal player (feature `player`, switched on by `gui`) and `mp3lame-encoder`
(LAME, LGPL) for the MP3 export (feature `mp3`, switched on by `gui`; it builds LAME from C sources, so it
needs the MSVC compiler); XML, MD5, WAV, MIDI and ID3 handling are written from scratch.

## Tests

```
cargo test                  # without real data
cargo test -- --ignored     # compare with a real library and a real stick
```

The regular tests work with small libraries in a temporary folder: sync plan, export and check,
atomic writing, WAV export, XML entities, blocked targets, import. The `#[ignore]` tests compare
byte for byte with the files uTool2 wrote to a real stick. The paths of the reference data come
from environment variables:

| Variable | Contents |
|---|---|
| `LP16_TEST_PROJECT_LIB` | project library with the reference songs and playlists |
| `LP16_TEST_AUDIO_LIB` | its audio library (`{$AudioLib}`) |
| `LP16_TEST_STICK` | root of a stick written by uTool2, e.g. `D:\` |

```powershell
$env:LP16_TEST_PROJECT_LIB = 'E:\LP16\CymaticProjectLib'
$env:LP16_TEST_AUDIO_LIB   = 'E:\LP16\AudioLibrary'
$env:LP16_TEST_STICK       = 'D:\'
cargo test -- --ignored
```

If a variable or a file is missing, the test fails instead of silently passing. Some tests expect
particular songs and playlists from the reference library (see `src/lib.rs`).

## What is missing and what is different

**LP-16 only.** Other devices known to uTool2 are not supported. The only Target Type is "LP-16".

**Not implemented yet:**

- Stereo songs and playlists (multitrack only)
- Pan per track in the song (the [song mix](#song-mix) edits level and mute; the pan stored in the
  `.songprj` is kept).
- Song editor: changing the name, sampling rate and resolution of an existing song
- Undo/redo, folders in the library ("New folder")
- Editing on the stick (Edit Target can only delete)
- Export Batch
- uTool2's import (from a folder or external drive, "Split at Markers")

**Different from the original:**

- **Import** is a batch import of songs from the AudioLibrary, see
  [Import](#import-batch-import-of-songs).
- **Quick Export** writes only what has changed. uTool2 rewrites every `song.set`.
- **Settings** only has the Library Folder; the export target is chosen in the Export tab.

Whether the LP-16 checks the `validationcode` is unknown; lp16-tool generates it the same way
uTool2 does.

## License

[GPL-3.0-or-later](LICENSE). The ASIO output uses the ASIO SDK, which Steinberg offers under
the GPLv3 (or a proprietary license); so that public releases can contain ASIO, lp16-tool itself
is licensed under the GPL-3.0-or-later since version 0.2.0. Versions up to 0.1.2 remain under the
MIT license.

ASIO is a trademark and software of Steinberg Media Technologies GmbH.

The licenses of the Rust crates lp16-tool is built from are listed in
[THIRD_PARTY_LICENSES.html](THIRD_PARTY_LICENSES.html); release archives include it next to
`LICENSE`. After changing dependencies, regenerate it (needs `cargo install cargo-about --features cli`):

```
cargo about generate --all-features about.hbs -o THIRD_PARTY_LICENSES.html
```

`about.toml` lists the accepted licenses, all of them GPLv3-compatible; a new dependency under any
other license makes the command fail.

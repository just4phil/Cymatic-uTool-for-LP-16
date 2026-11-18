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
- **Edit the library:** create, reorder, rename, copy and delete playlists; create songs and drag
  WAVs onto the 16 tracks in the song editor, choose the MIDI file.
- **Batch import of songs:** creates songs from new folders of the AudioLibrary in one go, see
  [Import](#import-batch-import-of-songs).
- **Edit Target:** the contents of the stick as a tree, with details on songs and playlists.

## Requirements

- Windows (developed and tested on Windows 11)
- [Rust](https://www.rust-lang.org/tools/install) (stable), only for building
- A uTool2 project library, or an empty folder that lp16-tool sets up

A ready-built exe is available under [Releases](../../releases).

## Build and run

```
cargo build --release
target\release\lp16-tool.exe
```

Without arguments (also by double-click) the user interface starts. To build without it:
`cargo build --release --no-default-features`.

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
# never written to, separate several targets with ;
protected_targets = "D:"
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

### Protecting the real stick

`protected_targets` blocks targets for every export, in the user interface and on the command
line. If the entry is missing, `D:` is blocked. This is on purpose: to try things out, export to
a copy of the stick. To export to the stick itself, remove its drive from the list
(`protected_targets = ""` blocks nothing). Back up `Recording\` before the first export to the
real stick.

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
| `src/sync.rs` | Stick Sync: `plan` (what an export would do), `run` (export and verify), `check_target` (blocked targets). Command line and user interface use the same process. |
| `src/main.rs` | Command line |
| `src/gui_app.rs` | User interface (egui 0.27) |

There are no dependencies apart from `eframe` and `rfd` for the user interface; XML, MD5, WAV and
MIDI handling are written from scratch.

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
- Mixer, i.e. level and pan per track (existing values from the `.songprj` are kept, though)
- Song editor: changing the name, sampling rate and resolution of an existing song
- Playlist editor: setting start mode and delay per song (existing values are kept)
- Undo/redo, folders in the library ("New folder")
- Deleting and editing on the stick (Edit Target only shows the contents)
- Export Batch
- uTool2's import (from a folder or external drive, "Split at Markers")

**Different from the original:**

- **Import** is a batch import of songs from the AudioLibrary, see
  [Import](#import-batch-import-of-songs).
- **Quick Export** writes only what has changed. uTool2 rewrites every `song.set`.
- **Settings** only has the Library Folder; the export target is chosen in the Export tab.
- `D:` is blocked for writing by default, see [Protecting the real stick](#protecting-the-real-stick).

Whether the LP-16 checks the `validationcode` is unknown; lp16-tool generates it the same way
uTool2 does.

## License

[MIT](LICENSE): free to use, modify and distribute, including commercially. The copyright and
license notice must be kept. No warranty.

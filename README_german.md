# lp16-tool

[English](README.md) · **Deutsch**

**Ersatz für uTool2: Songs und Playlists für den Cymatic Audio LP-16 verwalten und auf den Stick exportieren.**

Der Hersteller Cymatic Audio existiert nicht mehr, und mit ihm fehlt die Pflege für uTool2.
lp16-tool liest die uTool2-Projektbibliothek (`.songprj`, `.playlistprj`) und schreibt den Stick
(`Recording\<Song>\…`, `Recording\#PLAYLISTS\*.play`) **byte-gleich** so, wie uTool2 es tut.
Dazu gibt es eine Oberfläche, die der von uTool2 nachgebaut ist, und eine Kommandozeile.

> **Stand:** lp16-tool unterstützt **nur den LP-16** und ist **kein vollständiger Nachbau** von
> uTool2. Was fehlt oder anders funktioniert, steht unter
> [Was fehlt und was anders ist](#was-fehlt-und-was-anders-ist). Der Reiter **Import** macht
> etwas ganz anderes als im Original, siehe [Import](#import-batch-import-von-songs).

> lp16-tool ist ein unabhängiges Projekt und steht in keiner Verbindung zu Cymatic Audio.
> Es ist eine Neuentwicklung in Rust und enthält keinen Code aus uTool2. Die Dateiformate
> wurden aus dem Verhalten von uTool2 und den Dateien auf einem echten Stick abgeleitet.

## Funktionen

- **Stick-Export wie uTool2:** `song.set` (inklusive `validationcode`), `.play` und die WAVs
  (Header neu aufgebaut, Audiodaten unverändert, Namen `_<quelle>_<NN>.wav`) sind byte-gleich
  mit dem, was uTool2 schreibt.
- **Quick Export und Force Overwrite:** Quick Export schreibt nur neue und geänderte Songs,
  Force Overwrite schreibt alle ausgewählten Songs neu. Die Vorschau zeigt pro Song
  *New Export*, *Overwrite*, *Skip* oder *Force Overwrite* und die Datenmenge („To Copy“).
- **Sicher schreiben:** Jede Datei wird erst als `.lp16tmp` geschrieben und dann umbenannt.
  Nach jedem Song wird das Ergebnis auf dem Ziel geprüft. Ein abgebrochener Export hinterlässt
  nie eine halbe Datei.
- **Prüfen ohne zu schreiben:** `verify` vergleicht einen Stick mit der Bibliothek und endet mit
  Exit-Code 0 nur, wenn alles aktuell ist. Gut vor jedem Gig.
- **Bibliothek bearbeiten:** Playlists anlegen, umsortieren, umbenennen, kopieren und löschen;
  Songs anlegen und im Song-Editor WAVs per Drag & Drop auf die 16 Spuren ziehen, MIDI-Datei
  wählen.
- **Batch-Import von Songs:** legt aus neuen Ordnern der AudioLibrary auf einen Schlag Songs an,
  siehe [Import](#import-batch-import-von-songs).
- **Edit Target:** Inhalt des Sticks als Baum, mit Details zu Songs und Playlists.

## Voraussetzungen

- Windows (entwickelt und getestet unter Windows 11)
- [Rust](https://www.rust-lang.org/tools/install) (stable), nur zum Bauen
- Eine uTool2-Projektbibliothek oder ein leerer Ordner, den lp16-tool anlegt

Eine fertig gebaute exe gibt es unter [Releases](../../releases).

## Bauen und starten

```
cargo build --release
target\release\lp16-tool.exe
```

Ohne Argumente (auch per Doppelklick) startet die Oberfläche. Ohne die Oberfläche bauen:
`cargo build --release --no-default-features`.

## Einrichtung

Die Einstellungen stehen in `lp16-tool.toml`. Die Datei wird im aktuellen Ordner gesucht,
sonst neben der exe und in den Ordnern darüber. Als Vorlage dient
[`lp16-tool.example.toml`](lp16-tool.example.toml):

```
copy lp16-tool.example.toml lp16-tool.toml
```

`lp16-tool.toml` enthält eigene Pfade und ist deshalb nicht im Repo (`.gitignore`). Fehlt die
Datei, legt die Oberfläche sie beim ersten Ändern der Einstellungen neben der exe an. Am einfachsten setzt man
im Reiter **Settings** den **Library Folder**; „Initialize Folders“ legt fehlende Ordner an.

```toml
# Basisordner: <library>\CymaticProjectLib = Projektbibliothek,
#              <library>\AudioLibrary      = Audiodateien und Import
library = "E:\LP16_LIBRARY"
# Export-Ziel (wird im Export-Reiter gewählt und gemerkt)
stick_root = "F:\"
# optional: wird nie beschrieben, mehrere Ziele mit ; trennen
protected_targets = ""
```

Aufbau des Basisordners:

```
<library>\
├── CymaticProjectLib\
│   └── LP16\
│       ├── Multitrack Songs\       *.songprj
│       └── Multitrack Playlists\   *.playlistprj
└── AudioLibrary\
    └── <Song>\<YYYY-MM-DD…>\       *.wav, eine .mid
```

Ältere Dateien mit getrennten Pfaden (`audio_lib`, `project_lib`, `import_lib`) werden weiter
gelesen.

### Laufwerk schützen (optional)

Standardmäßig darf lp16-tool auf jedes Ziel schreiben. Mit `protected_targets` lassen sich
Laufwerke oder Ordner für jeden Export sperren, in der Oberfläche und in der Kommandozeile, zum
Beispiel `protected_targets = "G:"`, solange man mit einer Kopie des Sticks probiert. Mehrere
Einträge trennt man mit `;`. Vor dem ersten Export auf einen echten Stick `Recording\` sichern.

## Kommandozeile

```
lp16-tool song     <songprj>     <audio-lib> <ausgabe-ordner>
lp16-tool playlist <playlistprj> <project-lib> <audio-lib> <ausgabe-ordner>
lp16-tool export   <playlistprj> <project-lib> <audio-lib> <stick-ordner> [--dry-run] [--force]
lp16-tool verify   <playlistprj> <project-lib> <audio-lib> <stick-ordner>
lp16-tool gui
```

| Befehl | Was er tut |
|---|---|
| `song` | schreibt `<ausgabe>\<Song>\song.set` und zeigt den `validationcode` |
| `playlist` | schreibt `<ausgabe>\#PLAYLISTS\<Playlist>.play` |
| `export` | bringt eine Playlist mit allen Songs auf den Stick (`<stick>\Recording\…`); `--dry-run` zeigt nur an, `--force` = Force Overwrite |
| `verify` | prüft den Stick nur lesend; Exit-Code 0 = aktuell |

- `audio-lib` ersetzt `{$AudioLib}` in der `.songprj` (bei uTool2 `AudioFileLibraryPath` in
  `%APPDATA%\Cymatic Audio uTool2\Cymatic Audio uTool2.settings`).
- `project-lib` ist die Wurzel der Projektbibliothek und ersetzt `{$ProjectLib}` in der
  `.playlistprj`.

Beispiel:

```
lp16-tool verify ^
  "E:\LP16_LIBRARY\CymaticProjectLib\LP16\Multitrack Playlists\Set 2026.playlistprj" ^
  "E:\LP16_LIBRARY\CymaticProjectLib" ^
  "E:\LP16_LIBRARY\AudioLibrary" ^
  "F:\"
```

Eine `.songprj`, die nicht gelesen werden kann, bricht `export` ab, bevor etwas geschrieben
wird.

## Import: Batch-Import von Songs

Der Reiter **Import** hat mit dem Import von uTool2 nur den Namen gemeinsam.

- **uTool2** importiert Audio aus einem Ordner oder von einem externen Laufwerk in die
  Projektbibliothek, mit Optionen wie „Split at Markers“.
- **lp16-tool** legt Songs im **Stapel** an. Erwartet wird eine AudioLibrary mit einem Ordner pro
  Song und darin einem Ordner pro Version, benannt mit Datum:

  ```
  AudioLibrary\<Song>\<YYYY-MM-DD…>\   bis zu 16 Mono-WAVs, höchstens eine .mid
  ```

  „Import Songs“ durchsucht die AudioLibrary und schlägt alle Versionsordner vor, für die es noch
  keinen Song gibt und die neuer sind als die neueste Version in der Bibliothek. Nach der
  Bestätigung entsteht pro Ordner eine `.songprj` mit den WAVs auf den Spuren 1…n (natürlich
  sortiert) und der MIDI-Datei. Fehlerhafte Ordner (nicht mono, gemischte Formate, mehr als
  16 WAVs, mehrere MIDI-Dateien) werden angezeigt, aber nicht importiert. Vorhandene Dateien
  werden nie überschrieben.

**Update Playlist** gehört dazu: Es baut aus einer bestehenden Playlist eine neue, in der jeder
Song durch seine neueste Version ersetzt ist. Nach einem Import wird das passend vorgeschlagen.

## Wann ein Song neu exportiert wird

Quick Export schreibt einen Song, wenn

- sein Ordner auf dem Stick fehlt (*New Export*),
- eine WAV fehlt oder nicht mehr die Größe hat, die die aktuelle Quelle ergibt,
- die MIDI-Datei fehlt, oder
- die `song.set` abweicht. Der `validationcode` darin enthält Pfade und Änderungszeiten der
  Quelldateien, so fallen auch neu gerenderte WAVs gleicher Länge und geänderte MIDI-Dateien auf.

Sonst wird er übersprungen (*Skip*). Die `.play` einer ausgewählten Playlist wird bei jedem
Export geschrieben. Force Overwrite schreibt alle ausgewählten Songs neu; das hilft, wenn
Dateien auf dem Stick beschädigt sein könnten. Gelöscht wird auf dem Stick nichts.

## Aufbau des Codes

| Datei | Inhalt |
|---|---|
| `src/lib.rs` | Dateiformate: `SongProject` (eine `.songprj`, daraus `song.set` und `validationcode`), `.play`, `.playlistprj`, WAV- und MIDI-Lesen, Import, Einstellungen |
| `src/sync.rs` | Stick Sync: `plan` (was ein Export tun würde), `run` (exportieren und prüfen), `check_target` (gesperrte Ziele). Kommandozeile und Oberfläche nutzen denselben Ablauf. |
| `src/main.rs` | Kommandozeile |
| `src/gui_app.rs` | Oberfläche (egui 0.27) |

Es gibt keine Abhängigkeiten außer `eframe` und `rfd` für die Oberfläche; XML, MD5, WAV und MIDI
sind selbst geschrieben.

## Tests

```
cargo test                  # ohne echte Daten
cargo test -- --ignored     # Vergleich mit echter Bibliothek und echtem Stick
```

Die normalen Tests arbeiten mit kleinen Bibliotheken in einem temporären Ordner: Sync-Plan,
Export und Prüfung, atomares Schreiben, WAV-Export, XML-Entities, gesperrte Ziele, Import.
Die `#[ignore]`-Tests vergleichen byte-genau mit den Dateien, die uTool2 auf einen echten Stick
geschrieben hat. Die Pfade der Referenzdaten kommen aus Umgebungsvariablen:

| Variable | Inhalt |
|---|---|
| `LP16_TEST_PROJECT_LIB` | Projektbibliothek mit den Referenz-Songs und -Playlists |
| `LP16_TEST_AUDIO_LIB` | zugehörige Audio-Bibliothek (`{$AudioLib}`) |
| `LP16_TEST_STICK` | Wurzel eines Sticks, den uTool2 beschrieben hat, z. B. `D:\` |

```powershell
$env:LP16_TEST_PROJECT_LIB = 'E:\LP16\CymaticProjectLib'
$env:LP16_TEST_AUDIO_LIB   = 'E:\LP16\AudioLibrary'
$env:LP16_TEST_STICK       = 'D:\'
cargo test -- --ignored
```

Fehlt eine Variable oder eine Datei, schlägt der Test fehl, statt still „ok“ zu melden. Einige
Tests erwarten bestimmte Songs und Playlists aus der Referenzbibliothek (siehe `src/lib.rs`).

## Was fehlt und was anders ist

**Nur der LP-16.** Andere Geräte, die uTool2 kennt, werden nicht unterstützt. Als Target Type
gibt es nur „LP-16“.

**Noch nicht umgesetzt:**

- Stereo-Songs und -Playlists (nur Multitrack)
- Mixer bzw. Pegel und Panorama pro Spur (vorhandene Werte aus der `.songprj` werden aber
  übernommen)
- Song-Editor: Name, Sampling Rate und Resolution eines bestehenden Songs ändern
- Playlist-Editor: Start Mode und Delay pro Song einstellen (vorhandene Werte bleiben erhalten)
- Undo/Redo, Ordner in der Bibliothek („New folder“)
- Löschen und Bearbeiten auf dem Stick (Edit Target zeigt nur an)
- Export Batch
- Der Import von uTool2 (aus Ordner oder externem Laufwerk, „Split at Markers“)

**Anders als im Original:**

- **Import** ist ein Batch-Import von Songs aus der AudioLibrary, siehe
  [Import](#import-batch-import-von-songs).
- **Quick Export** schreibt nur, was sich geändert hat. uTool2 schreibt jede `song.set` neu.
- **Settings** kennt nur den Library Folder; das Export-Ziel wird im Export-Reiter gewählt.

Ob der LP-16 den `validationcode` prüft, ist nicht bekannt; lp16-tool erzeugt ihn wie uTool2.

## Lizenz

[MIT](LICENSE): frei nutzen, ändern und weitergeben, auch kommerziell. Der Copyright- und
Lizenzhinweis muss erhalten bleiben. Ohne Gewähr.

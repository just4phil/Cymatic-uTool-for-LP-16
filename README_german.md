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
- **Probe-Player:** spielt Songs und Playlists der Bibliothek über einen wählbaren
  Audioausgang ab, mit einem 16-Kanal-Proben-Mix, siehe [Probe-Player](#probe-player).

## Voraussetzungen

- Windows (entwickelt und getestet unter Windows 11)
- [Rust](https://www.rust-lang.org/tools/install) (stable), nur zum Bauen
- Eine uTool2-Projektbibliothek oder ein leerer Ordner, den lp16-tool anlegt

Eine fertig gebaute exe gibt es unter [Releases](../../releases). Sie enthält die ASIO-Ausgabe.

## Bauen und starten

```
cargo build --release
target\release\lp16-tool.exe
```

Ohne Argumente (auch per Doppelklick) startet die Oberfläche. Ohne die Oberfläche (und ohne die
Audio- und MIDI-Abhängigkeiten `cpal`, `rtrb` und `midir` des Probe-Players) bauen:
`cargo build --release --no-default-features`.

### Mit ASIO bauen

Der Standard-Build gibt über WASAPI aus und braucht weder das ASIO-SDK noch LLVM. Die
**ASIO**-Ausgabe (z. B. für den Behringer XR18 mit dem „X-AIR ASIO Driver“) ist das Cargo-Feature
`asio`. Öffentliche Releases enthalten es. Selbst bauen (Windows):

1. LLVM installieren (seine libclang wird zum Übersetzen der ASIO-Anbindung gebraucht):
   `winget install --id LLVM.LLVM -e`
2. Das ASIO-SDK von [steinberg.net/asiosdk](https://www.steinberg.net/asiosdk) herunterladen (die
   Version mit GPLv3-Option) und entpacken, z. B. nach `C:\SDKssiosdk`, sodass die Ordner
   `common`, `host` usw. direkt darin liegen.
3. Umgebungsvariablen setzen (für neue Shells; `setx` speichert sie für den Benutzer):
   ```
   setx LIBCLANG_PATH "C:\Program Files\LLVMin"
   setx CPAL_ASIO_DIR "C:\SDKssiosdk"
   ```
4. Bauen:
   ```
   cargo build --release --features asio
   ```

Mit dem Feature erscheint „ASIO“ in der Host-Liste im Reiter Settings, und die Geräteliste zeigt die
installierten ASIO-Treiber.

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
# Audioausgang des Probe-Players (Settings > Audio output, siehe unten)
audio_host = "WASAPI"
audio_device = ""
audio_out_left = "1"
audio_out_right = "2"
audio_buffer = "0"
# MIDI-Ausgang des Probe-Players (Settings > MIDI output); leer = aus
midi_port = ""
# Presets des Proben-Mix (vom Programm geschrieben, siehe „Proben-Mix“)
mix.1.name = "Default"
mix.1.master = "0.0"
mix.1.ch1 = "0.0 0 0 0"
mix_active = "Default"
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

Vor dem ersten Export auf einen echten Stick `Recording\` sichern.

## Probe-Player

Der Player spielt einen Song der Projektbibliothek über den **Audioausgang** ab, den man im Reiter
Settings wählt (WASAPI oder ASIO), zum Proben zu Hause. Er schickt außerdem die MIDI-Datei des Songs an einen
**MIDI-Ausgang** (siehe [MIDI-Ausgang](#midi-ausgang-reiter-settings)).

- Im Home-Reiter mit der rechten Maustaste auf einen Song klicken und **Play in Rehearsal**
  wählen. Das lädt den Song, wechselt in den Reiter **Rehearsal** und startet die Wiedergabe.
- Für eine Playlist: im Home-Reiter mit der rechten Maustaste auf sie klicken und **Play in
  Rehearsal** wählen, oder sie im Combo **Playlist** links im Reiter Rehearsal auswählen. Das lädt den
  ersten Song und wechselt in den Reiter; es startet **nicht** von selbst, weil der erste Song live
  auch auf die Taste wartet. Das Combo zeigt außerdem „(single song)“, solange ein einzelner Song
  geladen ist.
- Der Reiter zeigt Songname, Position und Länge, einen Fortschrittsbalken (Klicken oder Ziehen springt),
  Play/Pause, Stop und die Knöpfe "−10 s" / "+10 s". Ein Zähler erscheint, wenn der Audiopuffer leer lief (Aussetzer).
  Warnungen nennen Spur und Grund, z. B. eine fehlende oder nicht lesbare WAV. Der Song spielt
  trotzdem mit den übrigen Spuren.
- Springen während der Wiedergabe blendet über 5 ms aus und wieder ein, damit nichts knackt.
  Beim Ziehen des Fortschrittsbalkens zeigt die Zeit das Ziel; gesprungen wird beim Loslassen.
- Tastenkürzel gelten, solange der Reiter Rehearsal angezeigt wird und kein Textfeld den Fokus hat:

  | Taste | Funktion |
  |---|---|
  | Leertaste | Play/Pause |
  | Links / Rechts | −10 s / +10 s |
  | Umschalt + Links / Rechts | −30 s / +30 s |
  | Pos1 | auf 0 springen, der Zustand bleibt |
  | Esc | Stop |

  | Hoch / Runter | Markierung auf den vorigen / nächsten Song (wie ein Klick) |

  Dieselben Funktionen liegen auf den Knöpfen neben Play.
- **Playlists:** Die Songliste links zeigt Nummer, Name, Länge und ein Notensymbol, wenn der Song
  eine MIDI-Datei hat. Ein Song, der nicht gelesen werden kann (z. B. fehlende `.songprj`), ist rot,
  mit dem Grund als Tooltip, und wird übersprungen. **Genau ein Song ist immer markiert** (farbig
  hinterlegt); nach dem Laden einer Playlist ist es der erste spielbare. **Play startet den
  markierten Song** (ein geladener, pausierter Song läuft an seiner Position weiter).
  - **Einfacher Klick** markiert einen Song. Spielt nichts (gestoppt, pausiert, beendet, wartend,
    Countdown), wird der Song auf Position 0 geladen und bleibt gestoppt, damit Play ihn startet; ein
    laufender Countdown wird abgebrochen. Spielt gerade ein Song, läuft er weiter: Der angeklickte Song
    wird markiert und zum **nächsten** Song, kommt also nach dem Ende des aktuellen dran (ein Klick auf
    den spielenden Song nimmt das zurück). Der spielende Song hat ein ▶ vor dem Namen, der nächste einen
    Pfeil →.
  - **Doppelklick** startet den Song sofort.
  - Die Tasten Hoch und Runter verschieben die Markierung und klicken, Leertaste ist Play.
  - Endet ein Song, springt die Markierung auf den nächsten Song (den angeklickten, falls es einen
    gibt). Am Ende der Playlist bleibt sie auf dem letzten Song, der Zustand ist „beendet“; Play spielt
    diesen Song dann noch einmal.
  - Die Song-Auswahl im Mixer-Reiter ist dasselbe wie ein Klick.
  - Die Knöpfe ⏮ und ⏭ springen zum vorigen und nächsten Song:
    - Nächster: Läuft gerade ein Song, startet der nächste sofort; sonst wird er auf Position 0
      geladen und bleibt gestoppt. Beim letzten Song passiert nichts.
    - Voriger: Weiter als 3 s im Song springt er an seinen Anfang, sonst wird der Song davor geladen
      (und startet sofort, wenn der Player gespielt hat).
- **Autoplay after 5 seconds** (Kontrollkästchen über der Songliste, auch im Mixer-Reiter; dieselbe
  Einstellung, gespeichert als `rehearsal_autoplay` in `lp16-tool.toml`, standardmäßig aus). Aus: Endet
  ein Song, ist der nächste markiert und geladen, und der Player wartet (der Reiter zeigt „Next: <Song>
  – press Play“). An: Der nächste Song startet von selbst nach 5 Sekunden Stille (der Reiter zeigt
  „Next: <Song> in <n> s“). Play während des Countdowns startet sofort, Pause bricht ihn ab, Autoplay
  ausschalten beendet ihn. Die Einstellung ersetzt den Startmodus der Playlist: Der Player wertet
  `start_mode` und `start_mode_delay` in der `.playlistprj` nicht mehr aus (die Datei wird nicht
  verändert). Ein einzelner Song hat keinen nächsten.
- Der nächste Song wird geöffnet, sobald vom aktuellen weniger als 10 s übrig sind (und während
  des Wartens), damit er ohne spürbare Verzögerung startet. Hat er eine andere Samplerate, wird der
  Ausgang vor dem Start mit dieser Rate neu geöffnet.
- Wird die geladene Playlist im Playlist-Editor gespeichert, wird die Warteschlange neu aufgebaut;
  der spielende Song läuft an derselben Position weiter, wenn er noch in der Playlist ist, sonst
  stoppt der Player beim ersten Song. Eine umbenannte Playlist bleibt unter dem neuen Namen
  ausgewählt. Wird sie gelöscht, stoppt der Player und entlädt sie, mit einem Hinweis.
- Alle Spuren werden synchron von der Platte gestreamt (nichts wird vorab in den Speicher
  geladen) und zu Stereo gemischt, und zwar vom [Proben-Mix](#proben-mix); das Ergebnis wird auf
  Vollaussteuerung begrenzt.
- Der Stream wird mit der **Samplerate des Songs** geöffnet. Hat der nächste Song eine andere Rate,
  wird der Stream geschlossen und mit der neuen Rate neu geöffnet. Es gibt kein Resampling; der Song
  wird nur dann nicht gespielt, wenn das Gerät seine Rate nicht unterstützt (der Reiter sagt das).
- WAVs müssen mono sein, mit 8, 16, 24 oder 32 Bit Integer oder 32 Bit Float. Eine Spur mit
  anderer Samplerate als die erste lesbare Spur bleibt still.
- Wird ein Song umbenannt, gelöscht oder im Song-Editor gespeichert, während er in der geladenen
  Warteschlange ist, stoppt der Player und entlädt sie, und der Reiter fordert auf, sie neu zu laden.
- Export, Import und Bibliotheksbearbeitung bleiben während der Wiedergabe benutzbar.
- Ohne nutzbares Audiogerät zeigt der Reiter eine Meldung, und Play versucht es erneut. Verschwindet
  das Gerät während der Wiedergabe, stoppt sie, die Position bleibt und der Reiter meldet „Audio
  device lost“; nach dem Wiederanschließen genügt Play, der Ausgang wird neu geöffnet, ein Neustart ist
  nicht nötig.

### Proben-Mix

Ganz oben im Reiter Rehearsal (über Songliste und Player) steht der **Proben-Mix**: 16 Kanalzüge (LP-16-Ausgang 1 bis
16) und einen Master. Er gilt für alle Songs. Jede Spur des geladenen Songs spielt auf dem Kanalzug
ihres LP-16-Ausgangs (der Kanal in der `.songprj`); mehrere Spuren auf einem Ausgang werden in
diesem Kanalzug summiert. Eine Spur mit einem Ausgang jenseits von 16 bleibt still und erzeugt eine
Warnung. Der Kanalzug zeigt die Ausgangsnummer und den Spurnamen; ein Kanalzug ohne Spur ist
ausgegraut („—“), lässt sich aber trotzdem bedienen.

- Pro Kanalzug: Fader (−60 bis +6 dB, ganz unten „off“, Doppelklick = 0 dB), Panorama (−127 bis
  +127 wie in uTool2, Constant-Power-Gesetz, Doppelklick = Mitte), **M**ute und **S**olo (ein
  stummer Kanalzug bleibt auch mit Solo stumm) und eine Pegelanzeige mit dem Wert in dB.
- Master: Fader, Pegelanzeige links und rechts und eine **CLIP**-LED. Sie leuchtet, wenn der Master
  auf Vollaussteuerung begrenzen musste, und bleibt an, bis man sie anklickt.
- Die Pegelanzeigen zeigen den Spitzenwert nach Fader und Mute, vor dem Panorama; sie fallen mit
  20 dB pro Sekunde ab.
- Änderungen wirken sofort während der Wiedergabe und ohne Knacken (die Pegel laufen über 10 ms).
- **Presets** sind benannte Mixe: in der Liste wählen, **New** (neutral, „Mix N“), **Copy**,
  **Rename**, **Delete** (das letzte lässt sich nicht löschen) und **Reset** (zurück auf neutral:
  alle Kanalzüge 0 dB, Mitte, kein Mute/Solo, Master 0 dB). Namen müssen eindeutig und nicht leer
  sein und dürfen kein Anführungszeichen `"` und keinen Zeilenumbruch enthalten.
- Die Presets werden automatisch in `lp16-tool.toml` gespeichert (nach einer Änderung höchstens
  einmal pro Sekunde und immer beim Beenden) als `mix.<n>.name`, `mix.<n>.master`,
  `mix.<n>.ch<k> = "<Pegel> <Pan> <Mute> <Solo>"` (Pegel mit einer Nachkommastelle oder `off`) und
  `mix_active`.
- **Reiter Mixer:** Der Reiter nach Rehearsal zeigt denselben Proben-Mix im Stil des uTool2-Mixers:
  schmale, hohe Kanalzüge (S- und M-Taste, ein Pan-Drehregler, den man nach oben oder unten zieht,
  der Pegel in dB, ein hoher Fader mit dB-Skala und Pegelanzeige, Ausgangsnummer und Spurname) und
  der Master mit L/R-Pegel und CLIP-LED. Alle 16 Kanalzüge und der Master stehen nebeneinander; ein
  breiteres Fenster zeigt mehr davon, bei einem schmalen erscheint eine Scrollleiste. Oben ist die
  Preset-Leiste. Der Reiter bearbeitet denselben Mix, dasselbe Preset und dieselbe Config wie der
  kleine Mixer im Reiter Rehearsal und schreibt nie in Songs (es gibt also kein „Save to Song“).
  Unten liegt die Player-Leiste: Playlist- und Songauswahl, voriger/nächster Song, −10 s,
  Play/Pause, Stop, +10 s, der Fortschrittsbalken mit der Zeit und die Zeile „Next: ...“ mit
  Warnungen. Es ist derselbe Player wie im Reiter Rehearsal (einschließlich MIDI), und die
  Tastenkürzel gelten in beiden Reitern. Im Home-Reiter lädt die Taste **Mixer** eines Songs ihn in
  den Player (ohne zu starten) und öffnet den Reiter Mixer.
- **Der Proben-Mix wird nicht exportiert.** Er landet nie in einer `.songprj`, einer Playlist,
  einer `song.set` oder einer `.play` und nicht auf dem Stick. Die Mixer-Werte der Songs
  (`monitor_level`, `monitor_pan`, `monitor_mute`, der Kopfhörer-Mix des LP-16) bleiben
  unberührt und werden vom Player ignoriert.

### Audio output (Reiter Settings)

Der Bereich **Audio output** im Reiter Settings legt fest, wohin der Stereo-Mix geht. Änderungen
werden sofort in `lp16-tool.toml` gespeichert und beim nächsten Öffnen des Streams verwendet (ein
laufender Song wird nicht unterbrochen). Das Programm startet ohne Meldung, auch wenn das gespeicherte
Gerät fehlt; die Meldung kommt bei Play oder Test tone.

| Einstellung | Schlüssel in `lp16-tool.toml` | Bedeutung | Standard |
|---|---|---|---|
| Host | `audio_host` | Audiosystem: `WASAPI`, und `ASIO` in einem Build mit dem Feature `asio` | Standard-Host |
| Device | `audio_device` | Name des Ausgabegeräts | leer = Standardgerät |
| Left / Right | `audio_out_left`, `audio_out_right` | Ausgangskanäle (ab 1) für die linke und rechte Seite des Stereo-Mix; derselbe Kanal zweimal bekommt (L+R)/2 | 1 / 2 |
| Buffer | `audio_buffer` | Frames: 0 = Treiber-Standard, 64, 128, 256, 512, 1024, 2048 | 0 |

- **Refresh** liest die Geräteliste neu (z. B. nach dem Anstecken eines Geräts).
- **Test tone** spielt einen 1-kHz-Ton mit -20 dBFS, 1 s auf dem linken, dann 1 s auf dem rechten
  Kanal, mit den obigen Einstellungen (48 kHz, oder die Standardrate des Geräts, falls es 48 kHz nicht
  kann). Der Button ist deaktiviert, solange ein Song spielt.
- Läuft ein Stream, zeigt der Bereich „Running: <Rate> Hz, buffer <N> frames“. Lehnt das Gerät die
  gewählte Puffergröße ab, wird der Treiber-Standard benutzt und ein Hinweis sagt das.
- Ungültige Werte in der Datei (z. B. `audio_buffer = "333"`) werden durch den Standard ersetzt.

**ASIO** (Build mit `--features asio`): Die Geräteliste zeigt die installierten ASIO-Treiber, und
Left / Right wählen unter den Ausgangskanälen des Treibers (18 beim XR18). Die Samplerate des Songs
wird beim Treiber angefordert; lehnt er ab, sagt eine Meldung das. Nimmt der Treiber die Puffergröße
nicht an, gilt seine eigene Einstellung, und der Hinweis nennt die Einstellung im Control Panel des
ASIO-Treibers. Viele ASIO-Treiber bedienen nur eine Anwendung: Nutzt die DAW den Treiber, schlägt das
Öffnen mit einer Meldung fehl, die das nahelegt. Die Sample-Formate `i16`, `i32`, `f32` und `f64` des
Treibers werden alle unterstützt.

**Behringer XR18:** den Stereo-Mix auf zwei USB-Kanäle legen (z. B. 17 und 18, beim X-AIR-Treiber die
USB-Return-Kanäle) und in X Air Edit einen Kanal oder Aux-Return mit Quelle USB auf diese Kanäle routen.

### MIDI-Ausgang (Reiter Settings)

Im Bereich **MIDI output** des Reiters Settings wählt man den MIDI-Port, an den die MIDI-Datei des
Songs geschickt wird. Die Wahl wird sofort als `midi_port` in `lp16-tool.toml` gespeichert (leer =
kein MIDI-Ausgang, das ist der Standard; eine alte Datei ohne den Schlüssel bedeutet ebenfalls aus).

- Die Combo zeigt **(none)** und alle MIDI-Ausgänge des Systems (unter Windows gibt es immer
  „Microsoft GS Wavetable Synth“); **Refresh** liest die Liste neu.
- **Send test** schickt den Zustand, den ein Gerät direkt nach dem Start des geladenen Songs
  bekäme: Bank Select, Program Change und die Controller, die ganz am Songanfang liegen. Der Modeler
  bekommt so nur, was er beim Songstart ohnehin bekäme. Der Button braucht einen Port und einen
  geladenen Song mit MIDI-Datei.
- Der Port wird bei Play (oder beim Test) geöffnet, nicht beim Programmstart. Fehlt der gespeicherte
  Port oder lässt er sich nicht öffnen, steht im Reiter Rehearsal und in den Einstellungen „MIDI
  port not available: <Name>“, das Audio spielt trotzdem, und das nächste Play versucht es erneut.
- Der Reiter Rehearsal zeigt unter dem Audioausgang „MIDI: <Port> · <n> events“, mit einem Punkt,
  der bei jeder gesendeten Nachricht 150 ms aufleuchtet. „MIDI: no file“ heißt: Der Song hat keine
  (lesbare) MIDI-Datei, „MIDI: off“: Es ist kein Port gewählt. Die Songliste zeigt ein MIDI-Symbol
  bei Songs mit Datei.

**Wie das MIDI gesendet wird.** Die MIDI-Datei wird synchron zum Audio gesendet, Tempowechsel
eingerechnet. Die MIDI-Dateien der Band enthalten keine Noten und keine Clock, sondern Bank Select
(CC32) und Program Change auf Kanal 1 am Anfang, CC69 (Snapshot-Auswahl bei Line-6-Geräten) mit den
Werten 1 bis 5 über den Song verteilt und CC22 auf Kanal 10. Der Audio-Callback sendet nie selbst
MIDI: Er legt die Nachrichten mit einer Zielzeit in eine Warteschlange (Zeit des Callbacks +
Ausgangslatenz des Streams + Position im Block), und ein MIDI-Thread sendet sie pünktlich.

**Chasing.** Startet der Song an einer Position nach dem Anfang und bei jedem Sprung während der
Wiedergabe, wird zuerst der MIDI-Zustand an dieser Stelle gesendet, und zwar in dem Moment, in dem
das Audio zurückkommt: pro MIDI-Kanal Bank Select (CC0, CC32), das letzte Program Change und der
letzte Wert jedes anderen Controllers (1 bis 119). Noten, Pitch Bend, Aftertouch, SysEx und
Channel-Mode-Nachrichten werden nicht nachgeholt. Ein Event genau auf der Zielposition wird als
normales Event gesendet. Deshalb sorgen **CC69 und Program Change beim Springen für den richtigen
Snapshot**. Start bei 0 und Weiterspielen nach einer Pause senden jedes Event genau einmal, ohne
Chasing.

**All Notes Off** (CC123) geht auf jeden Kanal, auf dem die Datei Noten hat, bei Stop, Pause,
Sprung, Songwechsel und beim Beenden des Programms. Dateien ohne Noten senden nichts.

**Behringer XR18:** den XR18 über USB anschließen und hier seinen MIDI-Port wählen. Der XR18 gibt die
Nachrichten von seinem MIDI-Out an den Modeler weiter.

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
| `src/project_lib.rs` | Projektbibliothek: Songs und Playlists auflisten, anlegen, umbenennen, kopieren und löschen. Beim Umbenennen eines Songs zeigen die Playlists weiter auf ihn; ein Song, den eine Playlist nutzt, lässt sich nicht löschen. |
| `src/sync.rs` | Stick Sync: `plan` (was ein Export tun würde), `run` (exportieren und prüfen). Kommandozeile und Oberfläche nutzen denselben Ablauf. |
| `src/mix.rs` | Der Proben-Mix: Kanalzüge, Presets, Config-Zeilen, Speicher-Timer |
| `src/rehearsal.rs` | Abspiel-Engine des Probe-Players (Lesen, Mischen, Pegelanzeigen, Transport, Playlist-Warteschlange mit Markierung, Autoplay und Vorladen), unabhängig von jedem Audiogerät (Feature `player`) |
| `src/rehearsal_queue.rs` | Die Warteschlange des Players: Einträge, der Briefkasten, der Songs für die Engine öffnet, Zeilen der Songliste (Feature `player`) |
| `src/rehearsal_midi.rs` | MIDI der Engine: Events als Frames, die Nachrichten eines Blocks, Chasing (Feature `player`) |
| `src/midi_out.rs` | Adapter für den MIDI-Ausgang (`midir`): Ports, MIDI-Thread mit Zielzeiten, All Notes Off am Ende (Feature `player`) |
| `src/audio_out.rs` | Adapter für den Audioausgang: Hosts, Geräte und Streams über `cpal`, Kanalverteilung, Testton, Befehle und Status zwischen Oberfläche und Audio-Callback (Feature `player`) |
| `src/main.rs` | Kommandozeile |
| `src/gui_app.rs` | Oberfläche (egui 0.27) |
| `src/gui_rehearsal.rs` | Der Reiter Rehearsal |
| `src/gui_playlist.rs` | Playlist-Combo und Songliste im Reiter Rehearsal |
| `src/gui_mixer.rs` | Der Proben-Mix im Reiter Rehearsal, dazu Preset-Leiste und Pegel-Helfer, die der Reiter Mixer mitnutzt |
| `src/gui_mixer_tab.rs` | Der Reiter Mixer (Kanalzüge im uTool2-Stil, Transportleiste) |
| `src/gui_audio.rs` | Der Bereich „Audio output“ im Reiter Settings |
| `src/gui_midi.rs` | Der Bereich „MIDI output“ im Reiter Settings |

Es gibt keine Abhängigkeiten außer `eframe` und `rfd` für die Oberfläche sowie `cpal`, `rtrb` und `midir`
für den Probe-Player (Feature `player`, wird von `gui` eingeschaltet); XML, MD5, WAV und MIDI sind
selbst geschrieben.

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
- Mixer bzw. Pegel und Panorama pro Spur im Song (vorhandene Werte aus der `.songprj` werden
  aber übernommen). Der Probe-Player hat seinen eigenen Proben-Mix (siehe oben); der Song-Mixer bleibt
  nicht umgesetzt.
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

[GPL-3.0-or-later](LICENSE). Die ASIO-Ausgabe nutzt das ASIO-SDK, das Steinberg unter der GPLv3
(oder einer proprietären Lizenz) anbietet; damit öffentliche Releases ASIO enthalten können, steht
lp16-tool selbst seit Version 0.2.0 unter der GPL-3.0-or-later. Versionen bis 0.1.2 bleiben unter der
MIT-Lizenz.

ASIO is a trademark and software of Steinberg Media Technologies GmbH.

Die Lizenzen der Rust-Crates, aus denen lp16-tool gebaut wird, stehen in
[THIRD_PARTY_LICENSES.html](THIRD_PARTY_LICENSES.html); Release-Archive enthalten die Datei neben
`LICENSE`. Nach Änderungen an den Abhängigkeiten neu erzeugen (braucht
`cargo install cargo-about --features cli`):

```
cargo about generate --all-features about.hbs -o THIRD_PARTY_LICENSES.html
```

`about.toml` listet die akzeptierten Lizenzen, alle GPLv3-kompatibel; eine neue Abhängigkeit unter
einer anderen Lizenz lässt den Befehl fehlschlagen.

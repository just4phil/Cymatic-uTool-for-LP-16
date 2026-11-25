//! lp16-tool CLI - writes the files that a Cymatic Audio LP-16 expects.
//!
//! Usage:
//!   lp16-tool song     <songprj>     <audio-lib> <out-dir>
//!   lp16-tool playlist <playlistprj> <project-lib> <audio-lib> <out-dir>
//!   lp16-tool export   <playlistprj> <project-lib> <audio-lib> <stick-root> [--dry-run]
//!   lp16-tool verify   <playlistprj> <project-lib> <audio-lib> <stick-root>
//!   lp16-tool gui        (also: no arguments / double-click)
//!
//! audio-lib   is the AudioFileLibraryPath from uTool2.settings
//!             (replaces {$AudioLib} inside the .songprj).
//! project-lib is the project library root, e.g.
//!             E:\...\_CymaticProjectLib (replaces {$ProjectLib}
//!             inside the .playlistprj).

#[cfg(feature = "gui")]
mod gui_app;
#[cfg(feature = "gui")]
mod gui_audio;
#[cfg(feature = "gui")]
mod gui_backup;
#[cfg(feature = "gui")]
mod gui_bandpack;
#[cfg(feature = "gui")]
mod gui_midi;
#[cfg(feature = "gui")]
mod gui_mixer;
#[cfg(feature = "gui")]
mod gui_mixer_tab;
#[cfg(feature = "gui")]
mod gui_playlist;
#[cfg(feature = "gui")]
mod gui_rehearsal;
#[cfg(feature = "gui")]
mod gui_setlist_print;
#[cfg(feature = "gui")]
mod gui_tail;

#[cfg(feature = "gui")]
use crate::gui_app::run_gui;

use std::env;
use std::fs;
use std::path::Path;
use std::process::exit;

use lp16_tool::sync::{self, Action, ExportMode, Progress, SyncPlan};
#[cfg(feature = "gui")]
use lp16_tool::load_config;
use lp16_tool::{generate_play, now_ms, parse_playlist, SongProject};

fn die(msg: String) -> ! {
    eprintln!("{}", msg);
    exit(1);
}

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| die(format!("Cannot read {}: {}", path, e)))
}

fn write(path: &std::path::PathBuf, text: &str) {
    if let Some(dir) = path.parent() {
        if let Err(e) = fs::create_dir_all(dir) {
            die(format!("Cannot create {}: {}", dir.display(), e));
        }
    }
    if let Err(e) = fs::write(path, text.as_bytes()) {
        die(format!("Cannot write {}: {}", path.display(), e));
    }
}

/// Where the GUI stores its settings: lp16-tool.toml in the current
/// directory if it exists there, else the first one found next to the exe or
/// in a folder above it (target\release -> project folder), else next to the exe.
pub fn config_path() -> std::path::PathBuf {
    static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        const NAME: &str = "lp16-tool.toml";
        let cwd = std::path::PathBuf::from(NAME);
        if cwd.is_file() {
            return cwd;
        }
        let Some(exe_dir) = env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) else {
            return cwd;
        };
        exe_dir
            .ancestors()
            .map(|d| d.join(NAME))
            .find(|p| p.is_file())
            .unwrap_or_else(|| exe_dir.join(NAME))
    })
    .clone()
}

/// Started by double-click: Windows opened a console window just for this
/// process - close it so only the GUI window stays.
#[cfg(all(feature = "gui", windows))]
fn hide_own_console() {
    extern "system" {
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
        fn FreeConsole() -> i32;
    }
    unsafe {
        let mut ids = [0u32; 2];
        if GetConsoleProcessList(ids.as_mut_ptr(), 2) == 1 {
            FreeConsole();
        }
    }
}

#[cfg(feature = "gui")]
fn start_gui() {
    #[cfg(windows)]
    hide_own_console();
    // The GUI loads and saves lp16-tool.toml itself.
    run_gui(load_config(&config_path()));
}

fn load_song(songprj: &str, audio_lib: &str) -> SongProject {
    SongProject::load(songprj, audio_lib).unwrap_or_else(|e| die(format!("Cannot read {}", e)))
}

fn cmd_song(songprj: &str, audio_lib: &str, out_dir: &str) {
    let song = load_song(songprj, audio_lib);
    let file_path = Path::new(out_dir).join(&song.name).join("song.set");
    write(&file_path, &song.song_set());
    println!("validationcode: {}", song.validation_code());
    println!("wrote: {}", file_path.display());
}

fn cmd_playlist(playlistprj: &str, project_lib: &str, audio_lib: &str, out_dir: &str) {
    // The .play file does not need the audio files, only the .songprj
    // metadata. The parameter stays so the CLI shape is stable.
    let xml = read(playlistprj);
    let (name, songs) = parse_playlist(&xml, project_lib);

    let mut missing = 0usize;
    for s in &songs {
        if !Path::new(&s.songprj).exists() {
            eprintln!("MISSING song project: {}", s.songprj);
            missing += 1;
        }
    }
    if missing > 0 {
        die(format!("{} song project(s) missing, aborting", missing));
    }

    let play = generate_play(&name, now_ms(), &songs, |song| load_song(&song.songprj, audio_lib).duration());
    let file_path = Path::new(out_dir).join(lp16_tool::sync::PLAYLISTS).join(format!("{}.play", name));
    write(&file_path, &play);
    println!("{} song(s)", songs.len());
    println!("wrote: {}", file_path.display());
}

/// The sync plan for one .playlistprj: its songs and its .play.
fn plan_playlist(
    playlistprj: &str,
    project_lib: &str,
    audio_lib: &str,
    stick_root: &str,
    mode: ExportMode,
) -> SyncPlan {
    let (name, songs) = parse_playlist(&read(playlistprj), project_lib);
    let plan = sync::plan(stick_root, audio_lib, &[], &[(name, songs)], mode);
    for p in &plan.problems {
        eprintln!("MISSING {}", p);
    }
    plan
}

/// Warns on stderr if the LP-16 cannot read the file system of `stick_root`
/// (it only reads FAT32). Returns true if it warned.
fn warn_file_system(stick_root: &str) -> bool {
    match lp16_tool::volume::wrong_file_system(stick_root) {
        Some(fs) => {
            eprintln!(
                "WARNING {} is formatted {}: the LP-16 only reads FAT32 sticks and will reject it",
                stick_root, fs
            );
            true
        }
        None => false,
    }
}

/// verify: read-only check of a stick against the project library.
/// Exit code 0 only if every song and the .play are up to date and the
/// stick is FAT32 (or its file system is unknown).
fn cmd_verify(playlistprj: &str, project_lib: &str, audio_lib: &str, stick_root: &str) {
    let wrong_fs = warn_file_system(stick_root);
    let plan = plan_playlist(playlistprj, project_lib, audio_lib, stick_root, ExportMode::Quick);
    for s in &plan.songs {
        let marker = if s.plan.needs_export { "UPDATE" } else { "OK   " };
        println!("{} {} ({})", marker, s.plan.name, s.plan.reason);
    }
    for pl in &plan.playlists {
        let path = plan.stick.play_path(&pl.name);
        if pl.up_to_date {
            println!("OK    {} ({} song(s))", path.display(), pl.songs.len());
        } else {
            println!("UPDATE {} ({})", path.display(), pl.reason);
        }
    }
    let outdated = plan.songs.iter().filter(|s| s.plan.needs_export).count()
        + plan.playlists.iter().filter(|p| !p.up_to_date).count()
        + plan.problems.len();
    println!(
        "summary: {} song(s), {} up to date, {} need update",
        plan.songs.len(),
        plan.songs.iter().filter(|s| !s.plan.needs_export).count(),
        outdated
    );
    if !plan.is_up_to_date() {
        println!("{} problem(s) found", outdated);
        exit(1);
    }
    if wrong_fs {
        println!("stick is up to date, but not FAT32: the LP-16 will reject it");
        exit(1);
    }
    println!("stick is up to date");
}

fn cmd_export(
    playlistprj: &str,
    project_lib: &str,
    audio_lib: &str,
    stick_root: &str,
    dry_run: bool,
    force: bool,
) {
    // Quick Export writes only new/changed songs; Force Overwrite
    // rewrites everything (uTool2 fnd_23).
    let mode = if force { ExportMode::Force } else { ExportMode::Quick };
    warn_file_system(stick_root);
    let plan = plan_playlist(playlistprj, project_lib, audio_lib, stick_root, mode);
    if !plan.problems.is_empty() {
        die(format!("{} song project(s) missing, aborting", plan.problems.len()));
    }
    for s in &plan.songs {
        let marker = if s.action == Action::Skip { "OK    " } else { "EXPORT" };
        println!("{} {} ({})", marker, s.plan.name, s.plan.reason);
    }
    let play = |pl: &sync::PlannedPlaylist| plan.stick.play_path(&pl.name).display().to_string();
    if dry_run {
        for pl in &plan.playlists {
            println!("[DRY] {} ({} song(s))", play(pl), pl.songs.len());
        }
        return;
    }
    let lines = sync::run(&plan, |e| match e {
        Progress::Start(_) => true,
        Progress::Done(line) => {
            println!("       {}: {}", line.name, line.message);
            line.ok
        }
    })
    .unwrap_or_else(|e| die(e));
    if let Some(bad) = lines.iter().find(|l| !l.ok) {
        die(format!("Cannot export {}: {}", bad.name, bad.message));
    }
    for pl in &plan.playlists {
        println!("wrote {} ({} song(s))", play(pl), pl.songs.len());
    }
    let written = plan.songs.iter().filter(|s| s.action != Action::Skip).count();
    println!(
        "summary: {} song(s) in playlist, {} exported, {} up to date",
        plan.songs.len(),
        written,
        plan.songs.iter().filter(|s| !s.plan.needs_export).count()
    );
}

fn usage() -> ! {
    eprintln!("Usage:");
    eprintln!("  lp16-tool song     <songprj>     <audio-lib> <out-dir>");
    eprintln!("  lp16-tool playlist <playlistprj> <project-lib> <audio-lib> <out-dir>");
    eprintln!("  lp16-tool export   <playlistprj> <project-lib> <audio-lib> <stick-root> [--dry-run] [--force]");
    eprintln!("  lp16-tool verify   <playlistprj> <project-lib> <audio-lib> <stick-root>");
    eprintln!("  lp16-tool gui");
    eprintln!("  audio-lib   replaces {{$AudioLib}} inside the .songprj");
    eprintln!("  project-lib replaces {{$ProjectLib}} inside the .playlistprj");
    eprintln!("  export syncs the playlist's songs into <stick-root>\\Recording");
    eprintln!("  (like uTool2: missing or changed songs are exported)");
    eprintln!("  --force = Force Overwrite (rewrite everything), default is Quick Export");
    eprintln!("  verify checks a stick read-only and exits 0 only if it is up to date");
    eprintln!("  gui loads/saves paths from/to lp16-tool.toml");
    exit(1);
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let force = args.iter().any(|a| a == "--force");
    let args: Vec<String> = env::args()
        .filter(|a| a != "--dry-run" && a != "--force")
        .collect();
    match args.get(1).map(String::as_str) {
        Some("song") => {
            if args.len() != 5 {
                usage();
            }
            cmd_song(&args[2], &args[3], &args[4]);
        }
        Some("playlist") => {
            if args.len() != 6 {
                usage();
            }
            cmd_playlist(&args[2], &args[3], &args[4], &args[5]);
        }
        Some("export") => {
            if args.len() != 6 {
                usage();
            }
            cmd_export(&args[2], &args[3], &args[4], &args[5], dry_run, force);
        }
        Some("verify") => {
            if args.len() != 6 {
                usage();
            }
            cmd_verify(&args[2], &args[3], &args[4], &args[5]);
        }
        Some("gui") => {
            #[cfg(feature = "gui")]
            start_gui();
            #[cfg(not(feature = "gui"))]
            usage();
        }
        // no arguments (e.g. double-click on the exe) -> GUI
        #[cfg(feature = "gui")]
        None => start_gui(),
        _ => usage(),
    }
}
//! The audio output adapter of the rehearsal player: opens the default
//! output device of cpal's default host (WASAPI on Windows) and calls the
//! engine from its callback. Everything that can be tested without a device
//! (commands, sample conversion, channel layout) is plain functions and
//! types; `Player::open` is the only part that needs hardware.
//!
//! The audio callback owns the engine. The user interface talks to it with
//! `Command`s over an `mpsc` queue (taken with `try_recv` at the start of
//! every callback) and reads the status from an `Arc<Mutex<Status>>` that
//! the callback fills with `try_lock` after each block.

use crate::rehearsal::{Engine, LoadedSong, Status};
use crate::SongProject;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Stream;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// What the user interface can ask of the engine in the callback.
pub enum Command {
    /// a song that is already open (the files are opened on the caller's
    /// thread, the callback only swaps objects)
    Load(Box<LoadedSong>),
    Unload,
    Play,
    Pause,
    Stop,
    /// jump to a position in seconds (`Engine::seek`)
    Seek(f64),
}

/// A device sample format the engine's `f32` can be converted to.
pub trait OutSample: Copy {
    fn from_f32(v: f32) -> Self;
}

impl OutSample for f32 {
    fn from_f32(v: f32) -> f32 {
        v
    }
}

impl OutSample for i16 {
    fn from_f32(v: f32) -> i16 {
        (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
    }
}

impl OutSample for u16 {
    /// silence (0.0) is 32768
    fn from_f32(v: f32) -> u16 {
        (v.clamp(-1.0, 1.0) * 32767.0 + 32768.0).round() as u16
    }
}

/// Spread interleaved stereo (`stereo`) over a device buffer with
/// `channels` channels: L and R go to channels 1 and 2, the others get
/// silence; a mono device gets the mean of L and R.
pub fn write_frames<T: OutSample>(stereo: &[f32], out: &mut [T], channels: usize) {
    if channels == 0 {
        return;
    }
    for (frame, lr) in out.chunks_exact_mut(channels).zip(stereo.chunks_exact(2)) {
        if channels == 1 {
            frame[0] = T::from_f32((lr[0] + lr[1]) * 0.5);
        } else {
            frame[0] = T::from_f32(lr[0]);
            frame[1] = T::from_f32(lr[1]);
            for c in &mut frame[2..] {
                *c = T::from_f32(0.0);
            }
        }
    }
}

/// What runs in the audio callback.
pub struct Callback {
    engine: Engine,
    commands: Receiver<Command>,
    /// songs that were replaced: dropping one joins its thread, which must
    /// not happen in the callback
    trash: Sender<Box<LoadedSong>>,
    status: Arc<Mutex<Status>>,
    stereo: Vec<f32>,
}

impl Callback {
    pub fn new(
        engine: Engine,
        commands: Receiver<Command>,
        trash: Sender<Box<LoadedSong>>,
        status: Arc<Mutex<Status>>,
    ) -> Callback {
        Callback { engine, commands, trash, status, stereo: vec![0.0; 16384] }
    }

    fn apply(&mut self, cmd: Command) {
        let old = match cmd {
            Command::Load(song) => self.engine.set_song(*song),
            Command::Unload => self.engine.take_song(),
            Command::Play => {
                self.engine.play();
                None
            }
            Command::Pause => {
                self.engine.pause();
                None
            }
            Command::Stop => {
                self.engine.stop();
                None
            }
            Command::Seek(seconds) => {
                self.engine.seek(seconds);
                None
            }
        };
        if let Some(old) = old {
            let _ = self.trash.send(Box::new(old));
        }
    }

    /// One audio callback: commands, render, convert, status.
    pub fn fill<T: OutSample>(&mut self, out: &mut [T], channels: usize) {
        while let Ok(cmd) = self.commands.try_recv() {
            self.apply(cmd);
        }
        let need = out.len() / channels.max(1) * 2;
        if self.stereo.len() < need {
            self.stereo.resize(need, 0.0); // only if the device asks for more than usual
        }
        self.engine.render(&mut self.stereo[..need]);
        write_frames(&self.stereo[..need], out, channels);
        if let Ok(mut st) = self.status.try_lock() {
            *st = self.engine.status();
        }
    }
}

/// The open output device with its engine.
pub struct Player {
    commands: Sender<Command>,
    status: Arc<Mutex<Status>>,
    errors: Arc<Mutex<Vec<String>>>,
    stream: Option<Stream>,
    reaper: Option<JoinHandle<()>>,
    output_rate: u32,
    device_name: String,
}

impl Player {
    /// Open the default output device with its default configuration and
    /// start the stream. The error text is meant for the user.
    pub fn open() -> Result<Player, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("No audio output device found. Check the default playback device of Windows.")?;
        let device_name = device.name().unwrap_or_else(|_| "default device".to_string());
        let supported = device
            .default_output_config()
            .map_err(|e| format!("The audio output \"{}\" can't be used: {}", device_name, e))?;
        let format = supported.sample_format();
        let config = supported.config();
        let channels = config.channels as usize;
        let output_rate = config.sample_rate.0;

        let engine = Engine::new(output_rate);
        let status = Arc::new(Mutex::new(engine.status()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let (commands, rx) = channel();
        let (trash_tx, trash_rx) = channel::<Box<LoadedSong>>();
        let reaper = std::thread::Builder::new()
            .name("rehearsal-reaper".into())
            .spawn(move || {
                for song in trash_rx {
                    drop(song);
                }
            })
            .map_err(|e| e.to_string())?;
        let callback = Callback::new(engine, rx, trash_tx, status.clone());

        let err_list = errors.clone();
        let on_error = move |e: cpal::StreamError| {
            if let Ok(mut list) = err_list.lock() {
                let text = format!("Audio stream error: {}", e);
                if !list.contains(&text) {
                    list.push(text);
                }
            }
        };
        let stream = match format {
            cpal::SampleFormat::F32 => build::<f32>(&device, &config, callback, channels, on_error),
            cpal::SampleFormat::I16 => build::<i16>(&device, &config, callback, channels, on_error),
            cpal::SampleFormat::U16 => build::<u16>(&device, &config, callback, channels, on_error),
            other => Err(format!("the sample format {:?} is not supported", other)),
        }
        .map_err(|e| format!("The audio output \"{}\" could not be opened: {}", device_name, e))?;
        stream
            .play()
            .map_err(|e| format!("The audio output \"{}\" could not be started: {}", device_name, e))?;
        Ok(Player {
            commands,
            status,
            errors,
            stream: Some(stream),
            reaper: Some(reaper),
            output_rate,
            device_name,
        })
    }

    pub fn output_rate(&self) -> u32 {
        self.output_rate
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.commands.send(cmd);
    }

    /// Open the song's files (here, on the caller's thread) and hand it
    /// to the callback.
    pub fn load(&self, project: &SongProject) {
        self.send(Command::Load(Box::new(LoadedSong::open(project))));
    }

    /// The status of the last callback, plus errors of the stream.
    pub fn status(&self) -> Status {
        let mut st = match self.status.lock() {
            Ok(s) => s.clone(),
            Err(p) => p.into_inner().clone(),
        };
        if let Ok(errors) = self.errors.lock() {
            if !errors.is_empty() {
                Arc::make_mut(&mut st.warnings).extend(errors.iter().cloned());
            }
        }
        st
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // ends the callback and with it the engine and its reader thread
        drop(self.stream.take());
        if let Some(r) = self.reaper.take() {
            let _ = r.join();
        }
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut callback: Callback,
    channels: usize,
    on_error: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<Stream, String>
where
    T: cpal::SizedSample + OutSample,
{
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| callback.fill(data, channels),
            on_error,
            None,
        )
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rehearsal::tests::{constant, song, wav24, RATE};
    use crate::rehearsal::PlayState;
    use crate::tests::temp_dir;

    #[test]
    fn sample_conversion() {
        assert_eq!(<i16 as OutSample>::from_f32(0.0), 0);
        assert_eq!(<i16 as OutSample>::from_f32(1.0), 32767);
        assert_eq!(<i16 as OutSample>::from_f32(-1.0), -32767);
        assert_eq!(<i16 as OutSample>::from_f32(3.0), 32767);
        assert_eq!(<u16 as OutSample>::from_f32(0.0), 32768);
        assert_eq!(<u16 as OutSample>::from_f32(1.0), 65535);
        assert_eq!(<u16 as OutSample>::from_f32(-1.0), 1);
        assert_eq!(<u16 as OutSample>::from_f32(-5.0), 1);
    }

    #[test]
    fn channel_layout() {
        let stereo = [0.5, -0.5, 1.0, -1.0];
        let mut two = [9.0f32; 4];
        write_frames(&stereo, &mut two, 2);
        assert_eq!(two, stereo);
        let mut four = [9.0f32; 8];
        write_frames(&stereo, &mut four, 4);
        assert_eq!(four, [0.5, -0.5, 0.0, 0.0, 1.0, -1.0, 0.0, 0.0]);
        let mut one = [9.0f32; 2];
        write_frames(&stereo, &mut one, 1);
        assert_eq!(one, [0.0, 0.0]);
        let mut ints = [9i16; 4];
        write_frames(&stereo, &mut ints, 2);
        assert_eq!(ints, [16384, -16384, 32767, -32767]);
    }

    /// the callback without a device: commands in, samples and status out
    #[test]
    fn callback_runs_commands_and_publishes_the_status() {
        let dir = temp_dir("ao-callback");
        let a = wav24(&dir, "a", RATE, 1, &constant(2000, 0.25));
        let b = wav24(&dir, "b", RATE, 1, &constant(2000, 0.5));
        let (tx, rx) = channel();
        let (trash_tx, trash_rx) = channel();
        let engine = Engine::new(RATE);
        let status = Arc::new(Mutex::new(engine.status()));
        let mut cb = Callback::new(engine, rx, trash_tx, status.clone());

        let mut out = [1.0f32; 8];
        cb.fill(&mut out, 2);
        assert_eq!(out, [0.0; 8], "no song: silence");
        assert_eq!(status.lock().unwrap().state, PlayState::Empty);

        tx.send(Command::Load(Box::new(LoadedSong::open(&song(&dir, &[&a]))))).unwrap();
        tx.send(Command::Play).unwrap();
        // the callback may have to wait for the reader thread
        let mut out = [0.0f32; 8];
        for _ in 0..2000 {
            cb.fill(&mut out, 2);
            if out[0] != 0.0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!((out[0] - 0.25 * 0.707_106_77).abs() < 1e-6);
        assert_eq!(status.lock().unwrap().state, PlayState::Playing);
        assert!(status.lock().unwrap().position_frames >= 4);

        // replacing the song hands the old one to the trash, not to a join
        tx.send(Command::Load(Box::new(LoadedSong::open(&song(&dir, &[&b]))))).unwrap();
        cb.fill(&mut out, 2);
        let old = trash_rx.try_recv().expect("the old song goes to the trash");
        drop(old);
        assert_eq!(status.lock().unwrap().state, PlayState::Stopped);
        assert_eq!(status.lock().unwrap().position_frames, 0);

        tx.send(Command::Unload).unwrap();
        cb.fill(&mut out, 2);
        assert_eq!(status.lock().unwrap().state, PlayState::Empty);
        assert!(trash_rx.try_recv().is_ok());

        // a busy lock only skips one update
        let guard = status.lock().unwrap();
        cb.fill(&mut out, 2);
        drop(guard);
        drop(cb);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_seek_command_moves_the_position() {
        let dir = temp_dir("ao-seek");
        let a = wav24(&dir, "a", RATE, 1, &constant(48000, 0.25));
        let (tx, rx) = channel();
        let (trash_tx, _trash_rx) = channel();
        let engine = Engine::new(RATE);
        let status = Arc::new(Mutex::new(engine.status()));
        let mut cb = Callback::new(engine, rx, trash_tx, status.clone());
        tx.send(Command::Load(Box::new(LoadedSong::open(&song(&dir, &[&a]))))).unwrap();
        tx.send(Command::Seek(0.5)).unwrap();
        let mut out = [0.0f32; 8];
        cb.fill(&mut out, 2);
        let st = status.lock().unwrap().clone();
        assert_eq!((st.state, st.position_frames), (PlayState::Stopped, 24000));
        drop(cb);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn large_device_buffers_are_handled() {
        let (_tx, rx) = channel();
        let (trash_tx, _trash_rx) = channel();
        let engine = Engine::new(RATE);
        let status = Arc::new(Mutex::new(engine.status()));
        let mut cb = Callback::new(engine, rx, trash_tx, status);
        let mut out = vec![1i16; 40000 * 6];
        cb.fill(&mut out, 6);
        assert!(out.iter().all(|&s| s == 0));
    }
}

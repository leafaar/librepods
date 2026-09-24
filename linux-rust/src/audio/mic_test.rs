//! Record from the hi-res virtual source and play it back, so the user hears
//! exactly what apps get from the AirPods microphone.
//!
//! Recorder and Player each run a pulse stream on their own thread and share
//! only atomics and the PCM buffer with the UI, which polls them on its tick.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle, sleep};
use std::time::{Duration, Instant};

use libpulse_binding::def::{BufferAttr, Retval};
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::{
    FlagSet as StreamFlagSet, PeekResult, SeekMode, State as StreamState, Stream,
};

use crate::audio::eld::{ELD_CHANNELS, ELD_SAMPLE_RATE};
use crate::audio::output::{SOURCE_NAME, connect, wait_for};

/// Longest recording kept; about 38 MB of 64 kHz mono s16.
pub const MAX_RECORDING: Duration = Duration::from_secs(300);
/// How far the skip buttons jump.
pub const SKIP: Duration = Duration::from_secs(5);

const IDLE_SLEEP: Duration = Duration::from_millis(5);
/// When the recorder leaves, the monitor stops capture and resets the A2DP
/// transport (card profile off and back). A playback stream opened inside that
/// window is cut off, so the player waits this long after the recording ended.
const PLAYBACK_DELAY: Duration = Duration::from_secs(2);
/// Small playback buffer so pause and seek take effect quickly and the reported
/// position is close to what is being heard.
const PLAYBACK_BUFFER: Duration = Duration::from_millis(100);

const BYTES_PER_FRAME: usize = 2 * ELD_CHANNELS as usize;

fn spec() -> Spec {
    Spec {
        format: Format::S16le,
        channels: ELD_CHANNELS as u8,
        rate: ELD_SAMPLE_RATE,
    }
}

fn bytes_to_duration(bytes: usize) -> Duration {
    let frames = bytes / BYTES_PER_FRAME;
    Duration::from_secs_f64(frames as f64 / f64::from(ELD_SAMPLE_RATE))
}

fn duration_to_bytes(d: Duration) -> usize {
    let frames = (d.as_secs_f64() * f64::from(ELD_SAMPLE_RATE)) as usize;
    frames * BYTES_PER_FRAME
}

fn iterate(mainloop: &mut Mainloop, block: bool) -> Result<(), String> {
    match mainloop.iterate(block) {
        IterateResult::Quit(_) | IterateResult::Err(_) => {
            Err("Lost the connection to the sound server".to_string())
        }
        IterateResult::Success(_) => Ok(()),
    }
}

fn wait_ready(mainloop: &mut Mainloop, stream: &Stream) -> Result<(), String> {
    loop {
        iterate(mainloop, true)?;
        match stream.get_state() {
            StreamState::Ready => return Ok(()),
            StreamState::Failed | StreamState::Terminated => {
                return Err("The sound server refused the stream".to_string());
            }
            _ => {}
        }
    }
}

pub struct Recorder {
    stop: Arc<AtomicBool>,
    pcm: Arc<Mutex<Vec<u8>>>,
    thread: JoinHandle<Result<(), String>>,
}

impl Recorder {
    /// Start recording from the hi-res source until `stop` or MAX_RECORDING.
    pub fn start() -> Recorder {
        let stop = Arc::new(AtomicBool::new(false));
        let pcm = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let stop = stop.clone();
            let pcm = pcm.clone();
            thread::spawn(move || record_loop(&stop, &pcm))
        };
        Recorder { stop, pcm, thread }
    }

    pub fn elapsed(&self) -> Duration {
        bytes_to_duration(self.pcm.lock().map(|p| p.len()).unwrap_or(0))
    }

    /// The thread ended by itself: it hit MAX_RECORDING or failed.
    pub fn finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// Stop and return the recording. Returns quickly: the loop checks the flag
    /// every few milliseconds.
    pub fn stop(self) -> Result<Vec<u8>, String> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread
            .join()
            .map_err(|_| "The recorder crashed".to_string())??;
        let pcm = std::mem::take(&mut *self.pcm.lock().map_err(|_| "Recording lost")?);
        if pcm.is_empty() {
            return Err("No sound came from the AirPods microphone".to_string());
        }
        Ok(pcm)
    }
}

fn record_loop(stop: &AtomicBool, pcm: &Mutex<Vec<u8>>) -> Result<(), String> {
    let spec = spec();
    let max = duration_to_bytes(MAX_RECORDING);
    let (mut mainloop, mut context) =
        connect().ok_or_else(|| "Could not reach the sound server".to_string())?;
    let mut stream = Stream::new(&mut context, "Microphone test", &spec, None)
        .ok_or_else(|| "Could not create a recording stream".to_string())?;
    stream
        .connect_record(Some(SOURCE_NAME), None, StreamFlagSet::NOFLAGS)
        .map_err(|_| "The Hi-Res microphone input is not available".to_string())?;
    wait_ready(&mut mainloop, &stream)?;

    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        // Non-blocking so the stop flag is seen even when no audio arrives.
        if let Err(e) = iterate(&mut mainloop, false) {
            break Err(e);
        }
        match stream.peek() {
            Ok(PeekResult::Empty) => sleep(IDLE_SLEEP),
            Ok(PeekResult::Hole(_)) => {
                let _ = stream.discard();
            }
            Ok(PeekResult::Data(data)) => {
                let full = {
                    let mut pcm = pcm.lock().map_err(|_| "Recording lost".to_string())?;
                    let room = max.saturating_sub(pcm.len());
                    pcm.extend_from_slice(&data[..data.len().min(room)]);
                    pcm.len() >= max
                };
                let _ = stream.discard();
                if full {
                    break Ok(());
                }
            }
            Err(_) => break Err("Recording failed".to_string()),
        }
    };
    let _ = stream.disconnect();
    mainloop.quit(Retval(0));
    result
}

enum Command {
    Play,
    Pause,
    Seek(usize),
}

pub struct Player {
    commands: Sender<Command>,
    position: Arc<AtomicUsize>,
    playing: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    len: usize,
}

impl Player {
    /// Load a recording from `Recorder::stop`. Starts paused at the beginning.
    pub fn new(pcm: Vec<u8>) -> Player {
        let (commands, rx) = mpsc::channel();
        let position = Arc::new(AtomicUsize::new(0));
        let playing = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let len = pcm.len();
        {
            let position = position.clone();
            let playing = playing.clone();
            let error = error.clone();
            let ready_at = Instant::now() + PLAYBACK_DELAY;
            thread::spawn(move || {
                if let Err(e) = play_loop(&pcm, &rx, &position, &playing, ready_at) {
                    playing.store(false, Ordering::Relaxed);
                    if let Ok(mut slot) = error.lock() {
                        *slot = Some(e);
                    }
                }
            });
        }
        Player {
            commands,
            position,
            playing,
            error,
            len,
        }
    }

    pub fn play(&self) {
        // Replay from the start once the end was reached.
        if self.position.load(Ordering::Relaxed) >= self.len {
            let _ = self.commands.send(Command::Seek(0));
        }
        let _ = self.commands.send(Command::Play);
    }

    pub fn pause(&self) {
        let _ = self.commands.send(Command::Pause);
    }

    pub fn seek(&self, to: Duration) {
        let bytes = duration_to_bytes(to).min(self.len);
        // Update now so the slider does not jump back before the thread catches up.
        self.position.store(bytes, Ordering::Relaxed);
        let _ = self.commands.send(Command::Seek(bytes));
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn position(&self) -> Duration {
        bytes_to_duration(self.position.load(Ordering::Relaxed))
    }

    pub fn duration(&self) -> Duration {
        bytes_to_duration(self.len)
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|e| e.clone())
    }
}

// Dropping the Player drops the command sender, which ends the playback thread.

fn play_loop(
    pcm: &[u8],
    commands: &Receiver<Command>,
    position: &AtomicUsize,
    playing: &AtomicBool,
    ready_at: Instant,
) -> Result<(), String> {
    sleep(ready_at.saturating_duration_since(Instant::now()));

    let spec = spec();
    let (mut mainloop, mut context) =
        connect().ok_or_else(|| "Could not reach the sound server".to_string())?;
    let mut stream = Stream::new(&mut context, "Microphone test playback", &spec, None)
        .ok_or_else(|| "Could not create a playback stream".to_string())?;
    let target = u32::try_from(duration_to_bytes(PLAYBACK_BUFFER)).unwrap_or(u32::MAX);
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: target,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: u32::MAX,
    };
    stream
        .connect_playback(None, Some(&attr), StreamFlagSet::ADJUST_LATENCY, None, None)
        .map_err(|_| "Could not play on the default output".to_string())?;
    wait_ready(&mut mainloop, &stream)?;

    let mut offset = position.load(Ordering::Relaxed);
    loop {
        match commands.try_recv() {
            Ok(Command::Play) => playing.store(true, Ordering::Relaxed),
            Ok(Command::Pause) => {
                playing.store(false, Ordering::Relaxed);
                let op = stream.flush(None);
                wait_for(&mut mainloop, &op);
            }
            Ok(Command::Seek(to)) => {
                offset = to - to % BYTES_PER_FRAME;
                position.store(offset, Ordering::Relaxed);
                let op = stream.flush(None);
                wait_for(&mut mainloop, &op);
            }
            Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => {}
        }
        iterate(&mut mainloop, false)?;

        if !playing.load(Ordering::Relaxed) {
            sleep(IDLE_SLEEP);
            continue;
        }
        if offset >= pcm.len() {
            let op = stream.drain(None);
            wait_for(&mut mainloop, &op);
            playing.store(false, Ordering::Relaxed);
            position.store(pcm.len(), Ordering::Relaxed);
            continue;
        }
        let n = stream.writable_size().unwrap_or(0).min(pcm.len() - offset);
        if n == 0 {
            sleep(IDLE_SLEEP);
            continue;
        }
        stream
            .write(&pcm[offset..offset + n], None, 0, SeekMode::Relative)
            .map_err(|_| "Playback failed".to_string())?;
        offset += n;
        // Report what is being heard, not what was queued: back off by the buffer.
        let queued = duration_to_bytes(PLAYBACK_BUFFER);
        position.store(offset.saturating_sub(queued), Ordering::Relaxed);
    }
    let _ = stream.disconnect();
    mainloop.quit(Retval(0));
    Ok(())
}

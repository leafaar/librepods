//! Record from the hi-res virtual source and play it back, so the user hears
//! exactly what apps get from the AirPods microphone.
//!
//! Recorder and Player each run a pulse stream on their own thread and share
//! only atomics and the PCM buffer with the UI, which polls them on its tick.

use {
    crate::audio::{
        eld::{ELD_CHANNELS, ELD_SAMPLE_RATE},
        output::SOURCE_NAME,
        pulse::{connect, connect_cancellable, wait_for},
    },
    libpulse_binding::{
        def::{BufferAttr, Retval},
        mainloop::standard::{IterateResult, Mainloop},
        sample::{Format, Spec},
        stream::{FlagSet as StreamFlagSet, PeekResult, SeekMode, State as StreamState, Stream},
    },
    std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
        },
        thread::{self, JoinHandle, sleep},
        time::{Duration, Instant},
    },
    thiserror::Error,
};

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
/// Longest the sound server may take to make a new stream ready.
const READY_TIMEOUT: Duration = Duration::from_secs(3);

const BYTES_PER_FRAME: usize = 2 * ELD_CHANNELS as usize;

fn spec() -> Spec {
    Spec {
        format: Format::S16le,
        channels: ELD_CHANNELS as u8,
        rate: ELD_SAMPLE_RATE,
    }
}

/// Why the microphone test could not record or play. The UI shows the
/// message as is, so it is written for the user.
#[derive(Clone, Debug, Error)]
pub enum MicTestError {
    #[error("Could not reach the sound server")]
    SoundServerUnreachable,
    #[error("Lost the connection to the sound server")]
    ConnectionLost,
    #[error("The sound server did not open the stream in time")]
    StreamTimeout,
    #[error("The sound server refused the stream")]
    StreamRefused,
    #[error("Could not create a recording stream")]
    CreateRecordStream,
    #[error("Could not create a playback stream")]
    CreatePlaybackStream,
    #[error("The Hi-Res microphone input is not available")]
    SourceUnavailable,
    #[error("Could not play on the default output")]
    OutputUnavailable,
    #[error("Recording failed")]
    RecordingFailed,
    #[error("Playback failed")]
    PlaybackFailed,
    #[error("Recording lost")]
    RecordingLost,
    #[error("The recorder crashed")]
    RecorderCrashed,
    #[error("No sound came from the AirPods microphone")]
    NoSound,
    // io::Error is not Clone; the Player hands its error out by clone.
    #[error("Could not start the recorder: {0}")]
    StartRecorder(Arc<std::io::Error>),
    #[error("Could not start playback: {0}")]
    StartPlayback(Arc<std::io::Error>),
}

fn bytes_to_duration(bytes: usize) -> Duration {
    let frames = bytes / BYTES_PER_FRAME;
    Duration::from_secs_f64(frames as f64 / f64::from(ELD_SAMPLE_RATE))
}

/// Byte offset of `d` into the recording, always on a frame boundary.
fn duration_to_bytes(d: Duration) -> usize {
    let frames = (d.as_secs_f64() * f64::from(ELD_SAMPLE_RATE)) as usize;
    frames * BYTES_PER_FRAME
}

/// Round a byte offset down to the start of its frame.
fn align_to_frame(bytes: usize) -> usize {
    bytes - bytes % BYTES_PER_FRAME
}

/// Byte offset for a seek to `to`, clamped to the end of a `len` byte recording.
fn seek_offset(to: Duration, len: usize) -> usize {
    duration_to_bytes(to).min(len)
}

fn iterate(mainloop: &mut Mainloop, block: bool) -> Result<(), MicTestError> {
    match mainloop.iterate(block) {
        IterateResult::Quit(_) | IterateResult::Err(_) => Err(MicTestError::ConnectionLost),
        IterateResult::Success(_) => Ok(()),
    }
}

// Wait until the stream is ready, the server refuses it, READY_TIMEOUT passes or
// `cancelled` returns true (which returns Ok so the caller's loop sees its own
// stop condition). Polls rather than blocking in iterate(true), which would wait
// forever on a server that stopped answering.
fn wait_ready(
    mainloop: &mut Mainloop,
    stream: &Stream,
    cancelled: impl Fn() -> bool,
) -> Result<(), MicTestError> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if cancelled() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(MicTestError::StreamTimeout);
        }
        iterate(mainloop, false)?;
        match stream.get_state() {
            StreamState::Ready => return Ok(()),
            StreamState::Failed | StreamState::Terminated => {
                return Err(MicTestError::StreamRefused);
            },
            _ => sleep(IDLE_SLEEP),
        }
    }
}

pub struct Recorder {
    stop: Arc<AtomicBool>,
    pcm: Arc<Mutex<Vec<u8>>>,
    // Err holds why the recording thread could not be started.
    thread: Result<JoinHandle<Result<(), MicTestError>>, MicTestError>,
}

impl Recorder {
    /// Start recording from the hi-res source until `stop` or MAX_RECORDING.
    pub fn start() -> Recorder {
        let stop = Arc::new(AtomicBool::new(false));
        let pcm = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let stop = stop.clone();
            let pcm = pcm.clone();
            thread::Builder::new()
                .name("mic-test-record".into())
                .spawn(move || record_loop(&stop, &pcm))
                .map_err(|e| MicTestError::StartRecorder(Arc::new(e)))
        };
        Recorder { stop, pcm, thread }
    }

    pub fn elapsed(&self) -> Duration {
        bytes_to_duration(self.pcm.lock().map_or(0, |p| p.len()))
    }

    /// The thread ended by itself: it hit MAX_RECORDING or failed.
    pub fn finished(&self) -> bool {
        match &self.thread {
            Ok(handle) => handle.is_finished(),
            Err(_) => true,
        }
    }

    /// Stop and return the recording. Returns quickly: the thread checks the
    /// flag every few milliseconds, also while connecting to the sound server.
    pub fn stop(self) -> Result<Vec<u8>, MicTestError> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread?
            .join()
            .map_err(|_| MicTestError::RecorderCrashed)??;
        let pcm = std::mem::take(&mut *self.pcm.lock().map_err(|_| MicTestError::RecordingLost)?);
        if pcm.is_empty() {
            return Err(MicTestError::NoSound);
        }
        Ok(pcm)
    }
}

fn record_loop(stop: &AtomicBool, pcm: &Mutex<Vec<u8>>) -> Result<(), MicTestError> {
    let spec = spec();
    let max = duration_to_bytes(MAX_RECORDING);
    let stopped = || stop.load(Ordering::Relaxed);
    let Ok((mut mainloop, mut context)) = connect_cancellable(stopped) else {
        // Stopped while connecting: an empty recording, not a server error.
        if stopped() {
            return Ok(());
        }
        return Err(MicTestError::SoundServerUnreachable);
    };
    let mut stream = Stream::new(&mut context, "Microphone test", &spec, None)
        .ok_or(MicTestError::CreateRecordStream)?;
    stream
        .connect_record(Some(SOURCE_NAME), None, StreamFlagSet::NOFLAGS)
        .map_err(|_| MicTestError::SourceUnavailable)?;
    wait_ready(&mut mainloop, &stream, stopped)?;

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
            },
            Ok(PeekResult::Data(data)) => {
                let full = {
                    let mut pcm = pcm.lock().map_err(|_| MicTestError::RecordingLost)?;
                    let room = max.saturating_sub(pcm.len());
                    pcm.extend_from_slice(&data[..data.len().min(room)]);
                    pcm.len() >= max
                };
                let _ = stream.discard();
                if full {
                    break Ok(());
                }
            },
            Err(_) => break Err(MicTestError::RecordingFailed),
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
    error: Arc<Mutex<Option<MicTestError>>>,
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
            let thread_error = error.clone();
            let spawned = thread::Builder::new()
                .name("mic-test-play".into())
                .spawn(move || {
                    if let Err(e) = play_loop(&pcm, &rx, &position, &playing, ready_at) {
                        playing.store(false, Ordering::Relaxed);
                        if let Ok(mut slot) = thread_error.lock() {
                            *slot = Some(e);
                        }
                    }
                });
            if let Err(e) = spawned
                && let Ok(mut slot) = error.lock()
            {
                *slot = Some(MicTestError::StartPlayback(Arc::new(e)));
            }
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
        let bytes = seek_offset(to, self.len);
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

    pub fn error(&self) -> Option<MicTestError> {
        self.error.lock().ok().and_then(|e| e.clone())
    }
}

// Dropping the Player drops the command sender, which ends the playback thread.

// Wait until `ready_at`, queueing any commands that arrive meanwhile. Returns
// false if the Player was dropped, so no stream is opened for nobody.
fn wait_for_playback_slot(
    commands: &Receiver<Command>,
    ready_at: Instant,
    pending: &mut VecDeque<Command>,
) -> bool {
    loop {
        let left = ready_at.saturating_duration_since(Instant::now());
        if left.is_zero() {
            match commands.try_recv() {
                Ok(cmd) => pending.push_back(cmd),
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => return false,
            }
        } else {
            match commands.recv_timeout(left) {
                Ok(cmd) => pending.push_back(cmd),
                Err(RecvTimeoutError::Timeout) => {},
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
    }
}

fn play_loop(
    pcm: &[u8],
    commands: &Receiver<Command>,
    position: &AtomicUsize,
    playing: &AtomicBool,
    ready_at: Instant,
) -> Result<(), MicTestError> {
    let mut pending = VecDeque::new();
    if !wait_for_playback_slot(commands, ready_at, &mut pending) {
        return Ok(());
    }

    let spec = spec();
    let (mut mainloop, mut context) =
        connect().map_err(|_| MicTestError::SoundServerUnreachable)?;
    let mut stream = Stream::new(&mut context, "Microphone test playback", &spec, None)
        .ok_or(MicTestError::CreatePlaybackStream)?;
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
        .map_err(|_| MicTestError::OutputUnavailable)?;
    wait_ready(&mut mainloop, &stream, || false)?;

    let mut offset = position.load(Ordering::Relaxed);
    loop {
        let command = match pending.pop_front() {
            Some(cmd) => Ok(cmd),
            None => commands.try_recv(),
        };
        match command {
            Ok(Command::Play) => playing.store(true, Ordering::Relaxed),
            Ok(Command::Pause) => {
                playing.store(false, Ordering::Relaxed);
                let mut op = stream.flush(None);
                // A flush that did not finish only leaves some queued audio to
                // play out; the position below is right either way.
                let _ = wait_for(&mut mainloop, &mut op);
                // The flush dropped the queued audio that was not heard yet, so
                // resume from the heard position rather than the write offset.
                offset = align_to_frame(position.load(Ordering::Relaxed).min(pcm.len()));
            },
            Ok(Command::Seek(to)) => {
                offset = align_to_frame(to);
                position.store(offset, Ordering::Relaxed);
                let mut op = stream.flush(None);
                let _ = wait_for(&mut mainloop, &mut op);
            },
            Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => {},
        }
        iterate(&mut mainloop, false)?;

        if !playing.load(Ordering::Relaxed) {
            sleep(IDLE_SLEEP);
            continue;
        }
        if offset >= pcm.len() {
            let mut op = stream.drain(None);
            // An unfinished drain cuts off the last few milliseconds at most.
            let _ = wait_for(&mut mainloop, &mut op);
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
            .map_err(|_| MicTestError::PlaybackFailed)?;
        offset += n;
        // Report what is being heard, not what was queued: back off by the buffer.
        let queued = duration_to_bytes(PLAYBACK_BUFFER);
        position.store(offset.saturating_sub(queued), Ordering::Relaxed);
    }
    let _ = stream.disconnect();
    mainloop.quit(Retval(0));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_SECOND_BYTES: usize = ELD_SAMPLE_RATE as usize * BYTES_PER_FRAME;

    #[test]
    fn one_second_of_audio_converts_both_ways() {
        assert_eq!(duration_to_bytes(Duration::from_secs(1)), ONE_SECOND_BYTES);
        assert_eq!(bytes_to_duration(ONE_SECOND_BYTES), Duration::from_secs(1));
    }

    #[test]
    fn a_partial_frame_does_not_count_as_audio() {
        assert_eq!(bytes_to_duration(BYTES_PER_FRAME - 1), Duration::ZERO);
        assert_eq!(
            bytes_to_duration(BYTES_PER_FRAME * 64 + 1),
            Duration::from_millis(1)
        );
    }

    #[test]
    fn durations_convert_to_whole_frames() {
        // 1.5 samples at 64 kHz rounds down to one frame.
        let d = Duration::from_nanos(23_438);
        assert_eq!(duration_to_bytes(d), BYTES_PER_FRAME);
        assert_eq!(duration_to_bytes(d) % BYTES_PER_FRAME, 0);
    }

    #[test]
    fn offsets_align_down_to_the_frame_start() {
        assert_eq!(align_to_frame(0), 0);
        assert_eq!(align_to_frame(BYTES_PER_FRAME), BYTES_PER_FRAME);
        assert_eq!(
            align_to_frame(3 * BYTES_PER_FRAME + BYTES_PER_FRAME - 1),
            3 * BYTES_PER_FRAME
        );
    }

    #[test]
    fn seek_is_clamped_to_the_recording() {
        let len = 2 * ONE_SECOND_BYTES;

        assert_eq!(seek_offset(Duration::from_secs(1), len), ONE_SECOND_BYTES);
        assert_eq!(seek_offset(Duration::from_secs(10), len), len);
        assert_eq!(seek_offset(Duration::ZERO, len), 0);
    }

    #[test]
    fn max_recording_fits_in_memory_budget() {
        // About 38 MB of 64 kHz mono s16, as documented on MAX_RECORDING.
        assert_eq!(duration_to_bytes(MAX_RECORDING), 300 * ONE_SECOND_BYTES);
        assert!(duration_to_bytes(MAX_RECORDING) < 40_000_000);
    }

    #[test]
    fn errors_read_as_sentences_for_the_user() {
        assert_eq!(
            MicTestError::NoSound.to_string(),
            "No sound came from the AirPods microphone"
        );
        let io = std::io::Error::other("no threads");
        assert_eq!(
            MicTestError::StartRecorder(Arc::new(io)).to_string(),
            "Could not start the recorder: no threads"
        );
    }
}

//! Hi-res microphone lifecycle: a persistent virtual input device plus a monitor
//! that runs the AACP 0x58 capture only while an app is recording from it.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::{error, info, warn};
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::task::JoinHandle as TokioHandle;

use crate::audio::eld::{ELD_CHANNELS, ELD_FRAME_SAMPLES, ELD_SAMPLE_RATE, EldDecoder};
use crate::audio::output::{self, Output, VirtualMic};
use crate::bluetooth::aacp::AACPManager;
use crate::bluetooth::aacp_audio;

/// Delay after sending 0x58 START before resetting the A2DP transport
const A2DP_RESET_DELAY: Duration = Duration::from_millis(800);
/// How often the monitor polls the virtual source for activity
const POLL_INTERVAL: Duration = Duration::from_millis(400);
/// If no audio SDUs arrive for this long while a capture is active, the stream
/// is considered stalled and the capture is torn down and restarted.
const STALL_TIMEOUT: Duration = Duration::from_millis(2000);
/// Delay before the first capture retry after a stall or a failed start; every
/// further failure in a row doubles it.
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
/// Failed starts or stalls in a row after which the monitor stops retrying until
/// the recorder closes the microphone and opens it again.
const MAX_CAPTURE_FAILURES: u32 = 5;
/// A capture that delivered audio for this long before stalling was healthy, so
/// its stall starts the failure count over.
const HEALTHY_RUN: Duration = Duration::from_secs(30);
/// Consecutive AU decode errors (about 375 ms of audio) after which the decoder
/// is treated as wedged and recreated.
const DECODER_RESET_ERRORS: u32 = 50;
const LEVEL_RELEASE: f32 = 0.85;

#[derive(Clone, Default)]
pub struct MicStatus {
    level: Arc<AtomicU32>,
    active: Arc<AtomicBool>,
    app: Arc<Mutex<Option<String>>>,
    last_sdu: Arc<Mutex<Option<Instant>>>,
}

impl MicStatus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_level(&self, level: f32) {
        self.level.store(level.to_bits(), Ordering::Relaxed);
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    pub fn active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub fn app(&self) -> Option<String> {
        self.app.lock().unwrap().clone()
    }

    fn set_capture(&self, app: Option<String>) {
        self.active.store(app.is_some(), Ordering::Relaxed);
        *self.app.lock().unwrap() = app;
    }

    fn reset(&self) {
        self.active.store(false, Ordering::Relaxed);
        *self.app.lock().unwrap() = None;
        *self.last_sdu.lock().unwrap() = None;
        self.set_level(0.0);
    }

    /// Record that an audio SDU just arrived from the device.
    pub fn mark_sdu(&self) {
        *self.last_sdu.lock().unwrap() = Some(Instant::now());
    }

    /// Time since the last audio SDU, or None if none has arrived yet.
    fn since_last_sdu(&self) -> Option<Duration> {
        self.last_sdu.lock().unwrap().map(|t| t.elapsed())
    }
}

pub struct HiResMic {
    stop: Arc<Notify>,
    monitor: Option<TokioHandle<()>>,
}

impl HiResMic {
    // Create the persistent virtual input and spawn the activity monitor. The
    // monitor owns the virtual device and unloads it when it exits.
    pub async fn start(aacp: &AACPManager, addr: String, status: MicStatus) -> Option<HiResMic> {
        let vmic = VirtualMic::open(ELD_SAMPLE_RATE, ELD_CHANNELS as u8).await?;
        let stop = Arc::new(Notify::new());
        let wake = aacp.hires_wake();
        // Spawn on the backend runtime, not the caller's (the UI toggle uses a
        // throwaway runtime that would abort this task immediately).
        let monitor = aacp.runtime().spawn(monitor_loop(
            aacp.clone(),
            addr,
            status,
            stop.clone(),
            wake,
            vmic,
        ));
        Some(HiResMic {
            stop,
            monitor: Some(monitor),
        })
    }

    // Stop the monitor (tearing down any active capture) and unload the device.
    pub async fn stop(mut self) {
        self.stop.notify_one();
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.await;
        }
    }

    pub fn is_running(&self) -> bool {
        self.monitor.as_ref().is_some_and(|h| !h.is_finished())
    }
}

// A live capture session: the playback stream feeding the sink plus its decode
// thread, started when an app opens the mic.
struct Capture {
    decode_thread: JoinHandle<()>,
    // Delayed A2DP reset after START; aborted on stop so it cannot fire after
    // the stop-time reset.
    start_reset: TokioHandle<()>,
    // Aborting start_reset does not stop a reset already running on the blocking
    // pool, so stop_capture also sets this flag, which reset_a2dp checks before
    // each step.
    start_reset_cancel: Arc<AtomicBool>,
    started: Instant,
}

// Retry schedule for starting the capture while a recorder has the mic open:
// exponential backoff, then give up until the recorder leaves.
#[derive(Default)]
struct CaptureRetry {
    failures: u32,
    retry_at: Option<Instant>,
}

impl CaptureRetry {
    fn gave_up(&self) -> bool {
        self.failures >= MAX_CAPTURE_FAILURES
    }

    fn may_start(&self, now: Instant) -> bool {
        !self.gave_up() && self.retry_at.is_none_or(|at| now >= at)
    }

    // Record a failed start or a stall. Returns the delay before the next
    // attempt, or None once the monitor has given up.
    fn failed(&mut self, now: Instant) -> Option<Duration> {
        self.failures = self.failures.saturating_add(1);
        if self.gave_up() {
            self.retry_at = None;
            return None;
        }
        let delay = RETRY_BASE_DELAY.saturating_mul(1 << (self.failures - 1).min(16));
        self.retry_at = Some(now + delay);
        Some(delay)
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

fn capture_failed(retry: &mut CaptureRetry, what: &str) {
    match retry.failed(Instant::now()) {
        Some(delay) => warn!(
            "[hires] {} ({}/{}), retrying in {}s",
            what,
            retry.failures,
            MAX_CAPTURE_FAILURES,
            delay.as_secs()
        ),
        None => error!(
            "[hires] {} ({} failures in a row), giving up until the recorder closes and reopens the microphone",
            what, MAX_CAPTURE_FAILURES
        ),
    }
}

async fn monitor_loop(
    aacp: AACPManager,
    addr: String,
    status: MicStatus,
    stop: Arc<Notify>,
    wake: Arc<Notify>,
    vmic: VirtualMic,
) {
    let mut capture: Option<Capture> = None;
    let mut retry = CaptureRetry::default();
    // Conversation detection value saved while we override it off for capture.
    let mut old_convo_state: Option<bool> = None;
    info!(
        "[hires] activity monitor started, watching '{}'",
        output::SOURCE_NAME
    );

    loop {
        tokio::select! {
            _ = stop.notified() => break,
            _ = wake.notified() => {}
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }

        let app = tokio::task::spawn_blocking(|| output::source_consumer(output::SOURCE_NAME))
            .await
            .unwrap_or(None);
        let enabled = aacp.hires_mic_enabled();
        let recording = app.is_some();

        capture = match (enabled, recording, capture.take()) {
            (true, true, None) if retry.may_start(Instant::now()) => {
                info!("[hires] recorder detected ({:?}), starting capture", app);
                match start_capture(&aacp, &addr, &status).await {
                    Some(c) => {
                        status.set_capture(app);
                        // Only disable (and remember to restore) if it was on.
                        if old_convo_state.is_none()
                            && crate::utils::AppSettings::load().hires_mic_pause_convo
                            && aacp.conversation_detection_enabled().await
                        {
                            old_convo_state = Some(true);
                            aacp.set_conversation_detection(false).await;
                        }
                        Some(c)
                    }
                    None => {
                        status.reset();
                        capture_failed(&mut retry, "capture start failed");
                        None
                    }
                }
            }
            (true, true, Some(c)) => {
                status.set_capture(app);
                let stalled = status.since_last_sdu().is_some_and(|d| d > STALL_TIMEOUT);
                if stalled {
                    let healthy = c.started.elapsed() >= HEALTHY_RUN;
                    stop_capture(c, &aacp, &addr).await;
                    status.reset();
                    if healthy {
                        retry.reset();
                    }
                    capture_failed(
                        &mut retry,
                        &format!("no audio from device for {}ms", STALL_TIMEOUT.as_millis()),
                    );
                    None
                } else {
                    Some(c)
                }
            }
            // Disabled while capturing: end the 0x58 stream now.
            // The virtual source stays up feeding silence, so an active
            // recorder isn't dropped onto the AirPods' HFP mic (handsfree).
            (_, _, Some(c)) => {
                info!(
                    "[hires] stopping capture (enabled={}, recording={})",
                    enabled, recording
                );
                stop_capture(c, &aacp, &addr).await;
                if let Some(prev) = old_convo_state.take() {
                    aacp.set_conversation_detection(prev).await;
                }
                status.reset();
                None
            }
            (_, _, None) => None,
        };

        // A capture that failed to restart leaves no capture to stop, so the
        // arm above never runs; restore conversation detection once the
        // recorder is gone. While it is still recording we keep retrying.
        if capture.is_none()
            && !recording
            && let Some(prev) = old_convo_state.take()
        {
            aacp.set_conversation_detection(prev).await;
        }
        // The recorder left: its next open gets a fresh set of attempts.
        if !recording {
            retry.reset();
        }

        // Once disabled and nothing is recording from the source, unload it.
        if !enabled && !recording {
            break;
        }
    }

    if let Some(c) = capture.take() {
        stop_capture(c, &aacp, &addr).await;
    }
    if let Some(prev) = old_convo_state.take() {
        aacp.set_conversation_detection(prev).await;
    }
    status.reset();
    vmic.close().await;
}

async fn start_capture(aacp: &AACPManager, addr: &str, status: &MicStatus) -> Option<Capture> {
    let decoder = EldDecoder::new()?;
    let output = Output::open(ELD_SAMPLE_RATE, ELD_CHANNELS as u8)?;

    let rx = aacp.take_audio_channel().await;
    // Start the stall grace period now; the device should deliver SDUs (which
    // refresh this) well within STALL_TIMEOUT.
    status.mark_sdu();
    let Some(decode_thread) = spawn_decode_thread(rx, decoder, output, status.clone()) else {
        aacp.clear_audio_channel().await;
        return None;
    };

    if let Err(e) = aacp.send_start_audio().await {
        error!("failed to send 0x58 START: {}", e);
        aacp.clear_audio_channel().await;
        return None;
    }
    info!("[aacp] microphone stream started");

    let start_reset_cancel = Arc::new(AtomicBool::new(false));
    let start_reset = tokio::spawn({
        let cancel = start_reset_cancel.clone();
        let addr = addr.to_string();
        async move {
            tokio::time::sleep(A2DP_RESET_DELAY).await;
            let _ =
                tokio::task::spawn_blocking(move || output::reset_a2dp(&addr, Some(&cancel))).await;
        }
    });

    Some(Capture {
        decode_thread,
        start_reset,
        start_reset_cancel,
        started: Instant::now(),
    })
}

async fn stop_capture(capture: Capture, aacp: &AACPManager, addr: &str) {
    // The flag is the whole message, so Relaxed is enough.
    capture.start_reset_cancel.store(true, Ordering::Relaxed);
    capture.start_reset.abort();
    if let Err(e) = aacp.send_stop_audio().await {
        warn!("failed to send 0x58 STOP: {}", e);
    }
    aacp.clear_audio_channel().await;

    let handle = capture.decode_thread;
    let _ = tokio::task::spawn_blocking(move || handle.join()).await;

    // Waits in reset_a2dp for a start reset that is already switching the card.
    let addr = addr.to_string();
    let _ = tokio::task::spawn_blocking(move || output::reset_a2dp(&addr, None)).await;
}

fn spawn_decode_thread(
    rx: mpsc::Receiver<Vec<u8>>,
    decoder: EldDecoder,
    output: Output,
    status: MicStatus,
) -> Option<JoinHandle<()>> {
    match std::thread::Builder::new()
        .name("hires-decode".into())
        .spawn(move || decode_loop(rx, decoder, output, status))
    {
        Ok(handle) => Some(handle),
        Err(e) => {
            error!("could not spawn the hi-res decode thread: {}", e);
            None
        }
    }
}

fn decode_loop(
    mut rx: mpsc::Receiver<Vec<u8>>,
    mut decoder: EldDecoder,
    mut output: Output,
    status: MicStatus,
) {
    let mut frames: u64 = 0;
    let mut errors: u64 = 0;
    let mut consecutive_errors: u32 = 0;
    let mut env: f32 = 0.0;
    let mut pcm: Vec<i16> = Vec::with_capacity(4096);

    while let Some(sdu) = rx.blocking_recv() {
        pcm.clear();
        aacp_audio::demux_type58(&sdu, |au| {
            // An empty AU carries no audio, so there is nothing to decode or fill.
            if au.is_empty() {
                return;
            }
            match decoder.decode(au, &mut pcm) {
                Some(_) => {
                    frames += 1;
                    consecutive_errors = 0;
                }
                None => {
                    // insert a silent frame
                    errors += 1;
                    consecutive_errors += 1;
                    pcm.resize(pcm.len() + ELD_FRAME_SAMPLES * ELD_CHANNELS as usize, 0);
                    // A decoder stuck in a bad state fails every AU and would
                    // otherwise produce silence until the capture ends.
                    if consecutive_errors >= DECODER_RESET_ERRORS {
                        consecutive_errors = 0;
                        match EldDecoder::new() {
                            Some(fresh) => {
                                warn!(
                                    "[audio] {} decode errors in a row, recreated the AAC-ELD decoder",
                                    DECODER_RESET_ERRORS
                                );
                                decoder = fresh;
                            }
                            None => warn!(
                                "[audio] {} decode errors in a row and the AAC-ELD decoder could not be recreated",
                                DECODER_RESET_ERRORS
                            ),
                        }
                    }
                }
            }
        });

        let peak = if pcm.is_empty() {
            0.0
        } else {
            match output.write(&mut pcm) {
                Ok(peak) => peak,
                Err(()) => {
                    warn!("hi-res output broke; stopping decode loop");
                    break;
                }
            }
        };

        env = if peak >= env {
            peak
        } else {
            env * LEVEL_RELEASE
        };
        status.set_level(env);

        if frames > 0 && frames % 400 == 0 {
            let secs = frames as f64 * ELD_FRAME_SAMPLES as f64 / ELD_SAMPLE_RATE as f64;
            info!(
                "[audio] {} frames ({:.0}s), {} errors, level {:.2}",
                frames, secs, errors, env
            );
        }
    }
    status.set_level(0.0);
    info!(
        "[audio] hi-res decode loop ended ({} frames, {} errors)",
        frames, errors
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_retry_backs_off_then_gives_up() {
        let t0 = Instant::now();
        let mut retry = CaptureRetry::default();
        assert!(retry.may_start(t0));

        let mut now = t0;
        for attempt in 1..MAX_CAPTURE_FAILURES {
            let delay = retry.failed(now).expect("still retrying");
            assert_eq!(delay, RETRY_BASE_DELAY * (1 << (attempt - 1)));
            assert!(!retry.may_start(now + delay - Duration::from_millis(1)));
            now += delay;
            assert!(retry.may_start(now));
        }
        assert_eq!(retry.failed(now), None);
        assert!(!retry.may_start(now + Duration::from_secs(3600)));

        retry.reset();
        assert!(retry.may_start(now));
    }
}

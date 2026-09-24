//! PipeWire/PulseAudio output for the hi-res microphone

use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::introspect::SourceOutputInfo;
use libpulse_binding::context::{Context, FlagSet as ContextFlagSet};
use libpulse_binding::def::Retval;
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use libpulse_binding::operation::{Operation, State as OperationState};
use libpulse_binding::proplist::properties;
use log::{error, info, warn};
use std::cell::{Cell, RefCell};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dbus::blocking::Connection;
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;

use crate::audio::agc::Agc;

pub const SOURCE_NAME: &str = "AirPodsHiRes";

// O_NONBLOCK from asm-generic/fcntl.h, shared by x86 and arm64. Spelled out to avoid
// pulling in libc for one flag.
const O_NONBLOCK: i32 = 0o4000;

// Writes of at most PIPE_BUF bytes to a pipe are atomic: all or nothing, so a
// full pipe never leaves half a sample behind and the s16 stream stays aligned.
const PIPE_BUF: usize = 4096;

// FIFO the pipe-source reads from and that Output writes PCM into.
fn fifo_path() -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    format!("{dir}/librepods-hires.fifo")
}

pub struct VirtualMic {
    module: u32,
}

impl VirtualMic {
    /// Load the pipe-source. The sound server round trips block for up to
    /// PULSE_TIMEOUT each, so they run on the blocking pool, not on the async
    /// worker that awaits this.
    pub async fn open(sample_rate: u32, channels: u8) -> Option<VirtualMic> {
        tokio::task::spawn_blocking(move || Self::open_blocking(sample_rate, channels))
            .await
            .ok()
            .flatten()
    }

    /// Unload the pipe-source on the blocking pool. Dropping a VirtualMic
    /// instead does the same work on the current thread.
    pub async fn close(self) {
        let _ = tokio::task::spawn_blocking(move || drop(self)).await;
    }

    fn open_blocking(sample_rate: u32, channels: u8) -> Option<VirtualMic> {
        unload_stale_modules();

        let chan_map = if channels == 1 {
            "mono"
        } else {
            "front-left,front-right"
        };

        let fifo = fifo_path();
        let _ = std::fs::remove_file(&fifo); // drop any stale FIFO from a prior run

        let args = format!(
            "source_name={SOURCE_NAME} file={fifo} format=s16le rate={sample_rate} \
             channels={channels} channel_map={chan_map} \
             source_properties=\"device.description=AirPods_HiRes_Mic node.driver=false priority.driver=0\""
        );
        let module = match load_module("module-pipe-source", &args) {
            Some(i) => i,
            None => {
                warn!("could not load module-pipe-source");
                return None;
            }
        };

        info!(
            "[pw] hi-res mic ready: select '{}' as your microphone",
            SOURCE_NAME
        );
        Some(VirtualMic { module })
    }
}

impl Drop for VirtualMic {
    fn drop(&mut self) {
        unload_module(self.module);
        let _ = std::fs::remove_file(fifo_path());
    }
}

// Writes PCM into the pipe-source's FIFO. Opened only while an app is recording.
pub struct Output {
    fifo: File,
    agc: Option<Agc>,
}

impl Output {
    pub fn open(sample_rate: u32, _channels: u8) -> Option<Output> {
        // O_RDWR never blocks on a FIFO and keeps the pipe from ever seeing
        // "all writers closed"; we only ever write to it. O_NONBLOCK keeps the
        // decode thread from hanging when the source stops draining the pipe
        // (it suspends once the last recorder leaves, before the monitor notices).
        let path = fifo_path();
        let fifo = match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NONBLOCK)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                error!("could not open hi-res fifo {}: {}", path, e);
                return None;
            }
        };

        let agc = crate::utils::AppSettings::load()
            .hires_mic_agc
            .then(|| Agc::new(sample_rate));
        if agc.is_none() {
            info!("[pw] AGC disabled; passing through raw hi-res capture");
        }
        Some(Output { fifo, agc })
    }

    // Write s16 PCM into the FIFO, returning the (post-AGC) peak. AGC runs in
    // place on `pcm`.
    pub fn write(&mut self, pcm: &mut [i16]) -> Result<f32, ()> {
        if let Some(agc) = &mut self.agc {
            agc.process(pcm);
        }
        let pcm: &[i16] = pcm;

        let peak = pcm
            .iter()
            .map(|&s| (s as f32 / 32768.0).abs())
            .fold(0.0f32, f32::max);

        // SAFETY: the pointer and length cover exactly the initialised i16 slice,
        // u8 has no alignment requirement and every byte pattern is a valid u8.
        let bytes = unsafe {
            std::slice::from_raw_parts(pcm.as_ptr() as *const u8, std::mem::size_of_val(pcm))
        };

        'chunks: for chunk in bytes.chunks(PIPE_BUF) {
            loop {
                match self.fifo.write(chunk) {
                    Ok(_) => break,
                    // A signal arrived before anything was written: retry this chunk.
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    // Nobody is draining the pipe: drop the rest of this block
                    // rather than block the decode thread.
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break 'chunks,
                    Err(e) => {
                        error!("hi-res fifo write broke: {}", e);
                        return Err(());
                    }
                }
            }
        }
        Ok(peak)
    }
}

// Volume panels (pavucontrol's input tab, GNOME Settings' Sound panel) open a
// peak-detect record stream on every source just to draw its level bar. Those
// are not recorders and must not start the AirPods mic. libpulse does not report
// the PEAK_DETECT stream flag, so this is a heuristic on what the stream carries:
// - PulseAudio serves peak-detect streams with the "peaks" resampler;
// - PipeWire's pulse server marks them stream.monitor=true;
// - pavucontrol and libgvc (GNOME Settings) name the stream "Peak detect", a
//   translatable string, so their application ids are matched as well. Neither
//   app records audio, so no real recorder is lost by skipping them.
const LEVEL_METER_NAME: &str = "Peak detect";
const LEVEL_METER_APPS: [&str; 2] = ["org.PulseAudio.pavucontrol", "org.gnome.VolumeControl"];

fn is_level_meter(item: &SourceOutputInfo) -> bool {
    let props = &item.proplist;
    item.resample_method.as_deref() == Some("peaks")
        || props.get_str("stream.monitor").as_deref() == Some("true")
        || props.get_str(properties::MEDIA_NAME).as_deref() == Some(LEVEL_METER_NAME)
        || item.name.as_deref() == Some(LEVEL_METER_NAME)
        || props
            .get_str(properties::APPLICATION_ID)
            .is_some_and(|id| LEVEL_METER_APPS.contains(&id.as_str()))
}

// Name of the application recording from the virtual source, or None if idle.
// Corked (paused) streams and level meters do not count as recording.
pub fn source_consumer(name: &str) -> Option<String> {
    let (mut mainloop, context) = connect()?;
    let introspect = context.introspect();

    let index = Rc::new(Cell::new(u32::MAX));
    let mut op = introspect.get_source_info_by_name(name, {
        let index = index.clone();
        move |result| {
            if let ListResult::Item(item) = result {
                index.set(item.index);
            }
        }
    });
    wait_for(&mut mainloop, &mut op);

    let app = Rc::new(RefCell::new(None::<String>));
    let idx = index.get();
    if idx != u32::MAX {
        let mut op = introspect.get_source_output_info_list({
            let app = app.clone();
            move |result| {
                if let ListResult::Item(item) = result {
                    if item.source == idx
                        && !item.corked
                        && !is_level_meter(item)
                        && app.borrow().is_none()
                    {
                        let label = item
                            .proplist
                            .get_str("application.name")
                            .or_else(|| item.name.as_ref().map(|n| n.to_string()));
                        app.replace(label);
                    }
                }
            }
        });
        wait_for(&mut mainloop, &mut op);
    }
    mainloop.quit(Retval(0));

    let result = app.borrow().clone();
    result
}

// Serializes A2DP resets. A reset reads the active card profile, switches to
// "off" and back; if two ran at once, the second could read "off" as the profile
// to restore and leave the card off. Resets are started from different blocking
// threads (the delayed reset after a capture starts, and the one after it stops)
// that share no owner, and the card is a single system-wide resource, so one
// process-wide lock is the simplest correct guard.
static A2DP_RESET_LOCK: Mutex<()> = Mutex::new(());

// A2DP transport reset:
// We found that in some cases A2DP has to be suspended and resumed after a 0x58 mic start/stop
// to avoid a corrupted transport state of the airpods.
//
// `cancel` is checked before each step up to switching the card off; once the
// card is off it is always switched back.
pub fn reset_a2dp(bdaddr: &str, cancel: Option<&AtomicBool>) {
    // A flag with no other data riding on it, so Relaxed is enough.
    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    if !crate::utils::AppSettings::load().a2dp_reset {
        return;
    }
    // The guarded data is (), so a poisoned lock carries no broken state.
    let _guard = A2DP_RESET_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if cancelled() {
        return;
    }
    let card = format!("bluez_card.{}", bdaddr.replace(':', "_"));
    let Some((mut mainloop, context)) = connect() else {
        return;
    };
    let mut introspect = context.introspect();

    let current_profile = Rc::new(RefCell::new(None::<String>));
    let mut op = introspect.get_card_info_by_name(&card, {
        let current_profile = current_profile.clone();
        move |result| {
            if let ListResult::Item(item) = result {
                *current_profile.borrow_mut() = item
                    .active_profile
                    .as_ref()
                    .and_then(|p| p.name.as_ref())
                    .map(|n| n.to_string());
            }
        }
    });
    wait_for(&mut mainloop, &mut op);

    let Some(current_profile) = current_profile.borrow().clone() else {
        warn!("[pw] no active profile on {}; skipping A2DP reset", card);
        mainloop.quit(Retval(0));
        return;
    };
    // Already off: another reset was interrupted or the user turned the card
    // off. Restoring "off" would do nothing useful, so leave it alone.
    if current_profile == "off" {
        warn!("[pw] {} profile is off; skipping A2DP reset", card);
        mainloop.quit(Retval(0));
        return;
    }
    if cancelled() {
        mainloop.quit(Retval(0));
        return;
    }

    // Resetting the a2dp transport can pause media players do to setting the crad profile to off
    // Get all active media players
    let players = playing_media_players();
    if cancelled() {
        mainloop.quit(Retval(0));
        return;
    }

    info!(
        "[pw] reset A2DP transport: {} off -> {}",
        card, current_profile
    );
    let mut op = introspect.set_card_profile_by_name(&card, "off", None);
    wait_for(&mut mainloop, &mut op);

    let mut op = introspect.set_card_profile_by_name(&card, &current_profile, None);
    wait_for(&mut mainloop, &mut op);
    mainloop.quit(Retval(0));

    // resume all media players after the reset
    resume_media_players(&players);
}

// MPRIS players currently reporting "Playing" (kdeconnect proxies excluded).
pub(crate) fn playing_media_players() -> Vec<String> {
    let Ok(conn) = Connection::new_session() else {
        return Vec::new();
    };
    let proxy = conn.with_proxy(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        Duration::from_secs(5),
    );
    let names: (Vec<String>,) = match proxy.method_call("org.freedesktop.DBus", "ListNames", ()) {
        Ok(n) => n,
        Err(_) => return Vec::new(),
    };
    names
        .0
        .into_iter()
        .filter(|s| {
            s.starts_with("org.mpris.MediaPlayer2.")
                && !s.starts_with("org.mpris.MediaPlayer2.kdeconnect.mpris_")
        })
        .filter(|s| {
            let proxy = conn.with_proxy(s, "/org/mpris/MediaPlayer2", Duration::from_secs(5));
            proxy
                .get::<String>("org.mpris.MediaPlayer2.Player", "PlaybackStatus")
                .map(|st| st == "Playing")
                .unwrap_or(false)
        })
        .collect()
}

/// Pause every MPRIS player that is playing and return them, so the caller can
/// resume exactly those later.
pub(crate) fn pause_media_players() -> Vec<String> {
    let players = playing_media_players();
    let Ok(conn) = Connection::new_session() else {
        return Vec::new();
    };
    for service in &players {
        let proxy = conn.with_proxy(service, "/org/mpris/MediaPlayer2", Duration::from_secs(5));
        let _ =
            proxy.method_call::<(), _, &str, &str>("org.mpris.MediaPlayer2.Player", "Pause", ());
    }
    players
}

pub(crate) fn resume_media_players(services: &[String]) {
    if services.is_empty() {
        return;
    }
    let Ok(conn) = Connection::new_session() else {
        return;
    };
    for service in services {
        let proxy = conn.with_proxy(service, "/org/mpris/MediaPlayer2", Duration::from_secs(5));
        if proxy
            .method_call::<(), _, &str, &str>("org.mpris.MediaPlayer2.Player", "Play", ())
            .is_ok()
        {
            info!("[pw] resumed media player after A2DP reset: {}", service);
        }
    }
}

/// Longest a single sound server request may take before we give up on it.
const PULSE_TIMEOUT: Duration = Duration::from_secs(3);
const PULSE_POLL: Duration = Duration::from_millis(2);

/// Run the mainloop until `op` finishes. Returns false if it was cancelled, the
/// connection died, or the server did not answer within PULSE_TIMEOUT. Polls
/// instead of blocking in iterate(true), which would wait forever on a server
/// that stopped answering.
pub(crate) fn wait_for<T: ?Sized>(mainloop: &mut Mainloop, op: &mut Operation<T>) -> bool {
    let deadline = Instant::now() + PULSE_TIMEOUT;
    loop {
        match op.get_state() {
            OperationState::Done => return true,
            OperationState::Cancelled => return false,
            OperationState::Running => {}
        }
        if Instant::now() >= deadline {
            warn!("[pw] sound server did not answer in time");
            op.cancel();
            return false;
        }
        match mainloop.iterate(false) {
            IterateResult::Quit(_) | IterateResult::Err(_) => return false,
            IterateResult::Success(0) => std::thread::sleep(PULSE_POLL),
            IterateResult::Success(_) => {}
        }
    }
}

/// Connect to the sound server, giving up after PULSE_TIMEOUT.
pub(crate) fn connect() -> Option<(Mainloop, Context)> {
    let mut mainloop = Mainloop::new()?;
    let mut context = Context::new(&mainloop, "LibrePods")?;
    context
        .connect(None, ContextFlagSet::NOAUTOSPAWN, None)
        .ok()?;
    let deadline = Instant::now() + PULSE_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            warn!("[pw] could not connect to the sound server in time");
            return None;
        }
        match mainloop.iterate(false) {
            IterateResult::Quit(_) | IterateResult::Err(_) => return None,
            IterateResult::Success(0) => std::thread::sleep(PULSE_POLL),
            IterateResult::Success(_) => {}
        }
        match context.get_state() {
            libpulse_binding::context::State::Ready => break,
            libpulse_binding::context::State::Failed
            | libpulse_binding::context::State::Terminated => return None,
            _ => {}
        }
    }
    Some((mainloop, context))
}

fn unload_stale_modules() {
    let Some((mut mainloop, context)) = connect() else {
        return;
    };
    let stale: Rc<RefCell<Vec<u32>>> = Rc::new(RefCell::new(Vec::new()));
    let introspect = context.introspect();
    let mut op = introspect.get_module_info_list({
        let stale = stale.clone();
        move |result| {
            if let ListResult::Item(item) = result {
                if let Some(arg) = &item.argument {
                    if arg.contains(SOURCE_NAME) {
                        stale.borrow_mut().push(item.index);
                    }
                }
            }
        }
    });
    wait_for(&mut mainloop, &mut op);
    mainloop.quit(Retval(0));

    for index in stale.borrow().iter() {
        warn!("[pw] unloading stale hi-res module {}", index);
        unload_module(*index);
    }
}

fn load_module(name: &str, args: &str) -> Option<u32> {
    let (mut mainloop, context) = connect()?;
    let idx: Rc<Cell<u32>> = Rc::new(Cell::new(u32::MAX));
    let mut introspect = context.introspect();
    let mut op = introspect.load_module(name, args, {
        let idx = idx.clone();
        move |index| idx.set(index)
    });
    wait_for(&mut mainloop, &mut op);
    mainloop.quit(Retval(0));

    match idx.get() {
        u32::MAX => None,
        i => Some(i),
    }
}

fn unload_module(index: u32) {
    if index == u32::MAX {
        return;
    }
    if let Some((mut mainloop, context)) = connect() {
        let mut introspect = context.introspect();
        let mut op = introspect.unload_module(index, |_| {});
        wait_for(&mut mainloop, &mut op);
        mainloop.quit(Retval(0));
    }
}

//! PipeWire/PulseAudio output for the hi-res microphone

use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::{Context, FlagSet as ContextFlagSet};
use libpulse_binding::def::Retval;
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use libpulse_binding::operation::{Operation, State as OperationState};
use log::{error, info, warn};
use std::cell::{Cell, RefCell};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::rc::Rc;
use std::time::Duration;

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
    pub fn open(sample_rate: u32, channels: u8) -> Option<VirtualMic> {
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
    pub fn open(_sample_rate: u32, _channels: u8) -> Option<Output> {
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
            .then(Agc::new);
        if agc.is_none() {
            info!("[pw] AGC disabled; passing through raw hi-res capture");
        }
        Some(Output { fifo, agc })
    }

    // Write s16 PCM into the FIFO, returning the (post-AGC) peak
    pub fn write(&mut self, pcm: &[i16]) -> Result<f32, ()> {
        let processed;
        let pcm: &[i16] = if let Some(agc) = &mut self.agc {
            let mut buf = pcm.to_vec();
            agc.process(&mut buf);
            processed = buf;
            &processed
        } else {
            pcm
        };

        let peak = pcm
            .iter()
            .map(|&s| (s as f32 / 32768.0).abs())
            .fold(0.0f32, f32::max);

        // SAFETY: the pointer and length cover exactly the initialised i16 slice,
        // u8 has no alignment requirement and every byte pattern is a valid u8.
        let bytes = unsafe {
            std::slice::from_raw_parts(pcm.as_ptr() as *const u8, std::mem::size_of_val(pcm))
        };

        for chunk in bytes.chunks(PIPE_BUF) {
            match self.fifo.write(chunk) {
                Ok(_) => {}
                // Nobody is draining the pipe: drop the rest of this block
                // rather than block the decode thread.
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
                    break;
                }
                Err(e) => {
                    error!("hi-res fifo write broke: {}", e);
                    return Err(());
                }
            }
        }
        Ok(peak)
    }
}

// Name of the application recording from the virtual source, or None if idle.
pub fn source_consumer(name: &str) -> Option<String> {
    let (mut mainloop, context) = connect()?;
    let introspect = context.introspect();

    let index = Rc::new(Cell::new(u32::MAX));
    let op = introspect.get_source_info_by_name(name, {
        let index = index.clone();
        move |result| {
            if let ListResult::Item(item) = result {
                index.set(item.index);
            }
        }
    });
    wait_for(&mut mainloop, &op);

    let app = Rc::new(RefCell::new(None::<String>));
    let idx = index.get();
    if idx != u32::MAX {
        let op = introspect.get_source_output_info_list({
            let app = app.clone();
            move |result| {
                if let ListResult::Item(item) = result {
                    if item.source == idx && app.borrow().is_none() {
                        let label = item
                            .proplist
                            .get_str("application.name")
                            .or_else(|| item.name.as_ref().map(|n| n.to_string()));
                        app.replace(label);
                    }
                }
            }
        });
        wait_for(&mut mainloop, &op);
    }
    mainloop.quit(Retval(0));

    let result = app.borrow().clone();
    result
}

// A2DP transport reset:
// We found that in some cases A2DP has to be suspended and resumed after a 0x58 mic start/stop
// to avoid a corrupted transport state of the airpods.
pub fn reset_a2dp(bdaddr: &str) {
    if !crate::utils::AppSettings::load().a2dp_reset {
        return;
    }
    let card = format!("bluez_card.{}", bdaddr.replace(':', "_"));
    let Some((mut mainloop, context)) = connect() else {
        return;
    };
    let mut introspect = context.introspect();

    let current_profile = Rc::new(RefCell::new(None::<String>));
    let op = introspect.get_card_info_by_name(&card, {
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
    wait_for(&mut mainloop, &op);

    let Some(current_profile) = current_profile.borrow().clone() else {
        warn!("[pw] no active profile on {}; skipping A2DP reset", card);
        mainloop.quit(Retval(0));
        return;
    };

    // Resetting the a2dp transport can pause media players do to setting the crad profile to off
    // Get all active media players
    let players = playing_media_players();

    info!(
        "[pw] reset A2DP transport: {} off -> {}",
        card, current_profile
    );
    let op = introspect.set_card_profile_by_name(&card, "off", None);
    wait_for(&mut mainloop, &op);

    let op = introspect.set_card_profile_by_name(&card, &current_profile, None);
    wait_for(&mut mainloop, &op);
    mainloop.quit(Retval(0));

    // resume all media players after the reset
    resume_media_players(&players);
}

// MPRIS players currently reporting "Playing" (kdeconnect proxies excluded).
fn playing_media_players() -> Vec<String> {
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

fn resume_media_players(services: &[String]) {
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

// Block on the mainloop until `op` finishes. A blocking iterate sleeps until the
// server replies instead of spinning a core; a dead connection ends the wait.
fn wait_for<T: ?Sized>(mainloop: &mut Mainloop, op: &Operation<T>) {
    while op.get_state() == OperationState::Running {
        if let IterateResult::Quit(_) | IterateResult::Err(_) = mainloop.iterate(true) {
            break;
        }
    }
}

fn connect() -> Option<(Mainloop, Context)> {
    let mut mainloop = Mainloop::new()?;
    let mut context = Context::new(&mainloop, "LibrePods-HiResMic")?;
    context
        .connect(None, ContextFlagSet::NOAUTOSPAWN, None)
        .ok()?;
    loop {
        match mainloop.iterate(true) {
            IterateResult::Quit(_) | IterateResult::Err(_) => return None,
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
    let op = introspect.get_module_info_list({
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
    wait_for(&mut mainloop, &op);
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
    let op = introspect.load_module(name, args, {
        let idx = idx.clone();
        move |index| idx.set(index)
    });
    wait_for(&mut mainloop, &op);
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
        let op = introspect.unload_module(index, |_| {});
        wait_for(&mut mainloop, &op);
        mainloop.quit(Retval(0));
    }
}

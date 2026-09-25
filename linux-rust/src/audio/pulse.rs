//! Blocking requests to the PipeWire/PulseAudio sound server, and the
//! `SoundServer` seam the media controller drives cards and sinks through.

use {
    libpulse_binding::{
        callbacks::ListResult,
        context::{Context, FlagSet as ContextFlagSet, State as ContextState},
        def::Retval,
        mainloop::standard::{IterateResult, Mainloop},
        operation::{Operation, State as OperationState},
        proplist::Proplist,
        volume::{ChannelVolumes, Volume},
    },
    std::{
        cell::{Cell, RefCell},
        rc::Rc,
        time::{Duration, Instant},
    },
    thiserror::Error,
    tracing::{info, warn},
};

/// Longest a single sound server request may take before we give up on it.
const PULSE_TIMEOUT: Duration = Duration::from_secs(3);
const PULSE_POLL: Duration = Duration::from_millis(2);

/// A failed request to the sound server. The cause is part of the message
/// because these are logged with `{}` where they are handled.
#[derive(Debug, Error)]
pub enum SoundServerError {
    #[error("could not create a sound server connection")]
    Unavailable,
    #[error("the sound server refused the connection")]
    ConnectFailed,
    #[error("could not connect to the sound server within {PULSE_TIMEOUT:?}")]
    ConnectTimeout,
    #[error("cancelled while connecting to the sound server")]
    ConnectCancelled,
    #[error("the sound server did not answer within {PULSE_TIMEOUT:?}")]
    OperationTimeout,
    #[error("the sound server request was cancelled or the connection dropped")]
    OperationCancelled,
    #[error("no sound card for {0}")]
    CardNotFound(String),
    #[error("no sink for {0}")]
    SinkNotFound(String),
    #[error("the sound server rejected profile {profile} on card {card}")]
    ProfileRejected { card: u32, profile: String },
    #[error("could not load {0}")]
    ModuleLoadFailed(String),
}

/// Profiles of one sound card.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CardProfiles {
    pub active: Option<String>,
    pub available: Vec<String>,
}

impl CardProfiles {
    pub fn has(&self, profile: &str) -> bool {
        self.available.iter().any(|p| p == profile)
    }

    pub fn has_a2dp_sink(&self) -> bool {
        self.available.iter().any(|p| p.starts_with("a2dp-sink"))
    }
}

/// What the media controller needs from the sound server. Every call blocks
/// for up to a few seconds, so async callers run it on the blocking pool.
pub trait SoundServer: Send + Sync {
    /// Index of the card whose `device.string` contains `mac`.
    fn find_card(&self, mac: &str) -> Result<u32, SoundServerError>;
    fn card_profiles(&self, card: u32) -> Result<CardProfiles, SoundServerError>;
    fn set_card_profile(&self, card: u32, profile: &str) -> Result<(), SoundServerError>;
    /// Name of the sink that plays to the Bluetooth device `mac`.
    fn find_sink(&self, mac: &str) -> Result<String, SoundServerError>;
    /// Average volume over the sink's channels, in percent.
    fn sink_volume(&self, sink: &str) -> Result<u32, SoundServerError>;
    fn set_sink_volume(&self, sink: &str, percent: u32) -> Result<(), SoundServerError>;
}

/// The system's PipeWire or PulseAudio server, one connection per request.
pub struct PulseSoundServer;

struct OwnedCard {
    index: u32,
    proplist: Proplist,
    profiles: CardProfiles,
}

struct OwnedSink {
    name: Option<String>,
    proplist: Proplist,
    volume: ChannelVolumes,
}

impl SoundServer for PulseSoundServer {
    fn find_card(&self, mac: &str) -> Result<u32, SoundServerError> {
        cards()?
            .into_iter()
            .find_map(|card| {
                let device_string = card.proplist.get_str("device.string")?;
                device_string.contains(mac).then_some(card.index)
            })
            .ok_or_else(|| SoundServerError::CardNotFound(mac.to_string()))
    }

    fn card_profiles(&self, card: u32) -> Result<CardProfiles, SoundServerError> {
        cards()?
            .into_iter()
            .find(|c| c.index == card)
            .map(|c| c.profiles)
            .ok_or_else(|| SoundServerError::CardNotFound(format!("card {card}")))
    }

    fn set_card_profile(&self, card: u32, profile: &str) -> Result<(), SoundServerError> {
        let (mut mainloop, context) = connect()?;
        let mut introspector = context.introspect();
        let ok = Rc::new(Cell::new(false));
        let mut op = introspector.set_card_profile_by_index(
            card,
            profile,
            Some(Box::new({
                let ok = ok.clone();
                move |success| ok.set(success)
            })),
        );
        let finished = wait_for(&mut mainloop, &mut op);
        mainloop.quit(Retval(0));
        finished?;
        if ok.get() {
            Ok(())
        } else {
            Err(SoundServerError::ProfileRejected {
                card,
                profile: profile.to_string(),
            })
        }
    }

    fn find_sink(&self, mac: &str) -> Result<String, SoundServerError> {
        let sink = sinks()?.into_iter().find_map(|sink| {
            let name = sink.name?;
            let by_device_string = sink
                .proplist
                .get_str("device.string")
                .is_some_and(|d| d.to_uppercase().contains(&mac.to_uppercase()));
            let by_bluez_path = sink.proplist.get_str("bluez.path").is_some_and(|path| {
                path.split('/')
                    .next_back()
                    .unwrap_or("")
                    .replace("dev_", "")
                    .replace('_', ":")
                    .eq_ignore_ascii_case(mac)
            });
            (by_device_string || by_bluez_path).then_some(name)
        });
        match sink {
            Some(name) => {
                info!("Found sink name for MAC {}: {}", mac, name);
                Ok(name)
            },
            None => Err(SoundServerError::SinkNotFound(mac.to_string())),
        }
    }

    fn sink_volume(&self, sink: &str) -> Result<u32, SoundServerError> {
        let (mut mainloop, context) = connect()?;
        let info = sink_by_name(&mut mainloop, &context, sink);
        mainloop.quit(Retval(0));
        let volume = info?.volume;
        let channels = volume.len();
        if channels == 0 {
            return Err(SoundServerError::SinkNotFound(sink.to_string()));
        }
        let total: f64 = volume.get().iter().map(|v| f64::from(v.0)).sum();
        let percent =
            ((total / f64::from(channels) / f64::from(Volume::NORMAL.0)) * 100.0).round() as u32;
        Ok(percent)
    }

    fn set_sink_volume(&self, sink: &str, percent: u32) -> Result<(), SoundServerError> {
        let (mut mainloop, context) = connect()?;
        let result = sink_by_name(&mut mainloop, &context, sink).and_then(|info| {
            let mut volumes = ChannelVolumes::default();
            let raw = ((f64::from(percent) / 100.0) * f64::from(Volume::NORMAL.0)).round() as u32;
            volumes.set(info.volume.len(), Volume(raw));
            let mut op = context
                .introspect()
                .set_sink_volume_by_name(sink, &volumes, None);
            wait_for(&mut mainloop, &mut op)
        });
        mainloop.quit(Retval(0));
        result
    }
}

fn cards() -> Result<Vec<OwnedCard>, SoundServerError> {
    let (mut mainloop, context) = connect()?;
    let cards: Rc<RefCell<Vec<OwnedCard>>> = Rc::new(RefCell::new(Vec::new()));
    let mut op = context.introspect().get_card_info_list({
        let cards = cards.clone();
        move |result| {
            if let ListResult::Item(item) = result {
                let available = item
                    .profiles
                    .iter()
                    .filter_map(|p| p.name.as_ref().map(ToString::to_string))
                    .collect();
                let active = item
                    .active_profile
                    .as_ref()
                    .and_then(|p| p.name.as_ref().map(ToString::to_string));
                cards.borrow_mut().push(OwnedCard {
                    index: item.index,
                    proplist: item.proplist.clone(),
                    profiles: CardProfiles { active, available },
                });
            }
        }
    });
    let finished = wait_for(&mut mainloop, &mut op);
    mainloop.quit(Retval(0));
    finished?;
    // A cancelled operation leaks its callback and with it a clone of `cards`,
    // so take the contents instead of unwrapping the Rc.
    Ok(std::mem::take(&mut *cards.borrow_mut()))
}

fn sinks() -> Result<Vec<OwnedSink>, SoundServerError> {
    let (mut mainloop, context) = connect()?;
    let sinks: Rc<RefCell<Vec<OwnedSink>>> = Rc::new(RefCell::new(Vec::new()));
    let mut op = context.introspect().get_sink_info_list({
        let sinks = sinks.clone();
        move |result| {
            if let ListResult::Item(item) = result {
                sinks.borrow_mut().push(OwnedSink {
                    name: item.name.as_ref().map(ToString::to_string),
                    proplist: item.proplist.clone(),
                    volume: item.volume,
                });
            }
        }
    });
    let finished = wait_for(&mut mainloop, &mut op);
    mainloop.quit(Retval(0));
    finished?;
    Ok(std::mem::take(&mut *sinks.borrow_mut()))
}

fn sink_by_name(
    mainloop: &mut Mainloop,
    context: &Context,
    name: &str,
) -> Result<OwnedSink, SoundServerError> {
    let sink: Rc<RefCell<Option<OwnedSink>>> = Rc::new(RefCell::new(None));
    let mut op = context.introspect().get_sink_info_by_name(name, {
        let sink = sink.clone();
        move |result| {
            if let ListResult::Item(item) = result {
                *sink.borrow_mut() = Some(OwnedSink {
                    name: item.name.as_ref().map(ToString::to_string),
                    proplist: item.proplist.clone(),
                    volume: item.volume,
                });
            }
        }
    });
    wait_for(mainloop, &mut op)?;
    sink.borrow_mut()
        .take()
        .ok_or_else(|| SoundServerError::SinkNotFound(name.to_string()))
}

/// Run the mainloop until `op` finishes. Fails if it was cancelled, the
/// connection died, or the server did not answer within PULSE_TIMEOUT. Polls
/// instead of blocking in iterate(true), which would wait forever on a server
/// that stopped answering.
pub(crate) fn wait_for<T: ?Sized>(
    mainloop: &mut Mainloop,
    op: &mut Operation<T>,
) -> Result<(), SoundServerError> {
    let deadline = Instant::now() + PULSE_TIMEOUT;
    loop {
        match op.get_state() {
            OperationState::Done => return Ok(()),
            OperationState::Cancelled => return Err(SoundServerError::OperationCancelled),
            OperationState::Running => {},
        }
        if Instant::now() >= deadline {
            warn!("[pw] sound server did not answer in time");
            op.cancel();
            return Err(SoundServerError::OperationTimeout);
        }
        match mainloop.iterate(false) {
            IterateResult::Quit(_) | IterateResult::Err(_) => {
                return Err(SoundServerError::OperationCancelled);
            },
            IterateResult::Success(0) => std::thread::sleep(PULSE_POLL),
            IterateResult::Success(_) => {},
        }
    }
}

/// Connect to the sound server, giving up after PULSE_TIMEOUT.
pub(crate) fn connect() -> Result<(Mainloop, Context), SoundServerError> {
    connect_cancellable(|| false)
}

/// Like connect(), but also gives up as soon as `cancelled` returns true.
pub(crate) fn connect_cancellable(
    cancelled: impl Fn() -> bool,
) -> Result<(Mainloop, Context), SoundServerError> {
    let mut mainloop = Mainloop::new().ok_or(SoundServerError::Unavailable)?;
    let mut context = Context::new(&mainloop, "LibrePods").ok_or(SoundServerError::Unavailable)?;
    context
        .connect(None, ContextFlagSet::NOAUTOSPAWN, None)
        .map_err(|_| SoundServerError::ConnectFailed)?;
    let deadline = Instant::now() + PULSE_TIMEOUT;
    loop {
        if cancelled() {
            return Err(SoundServerError::ConnectCancelled);
        }
        if Instant::now() >= deadline {
            warn!("[pw] could not connect to the sound server in time");
            return Err(SoundServerError::ConnectTimeout);
        }
        match mainloop.iterate(false) {
            IterateResult::Quit(_) | IterateResult::Err(_) => {
                return Err(SoundServerError::ConnectFailed);
            },
            IterateResult::Success(0) => std::thread::sleep(PULSE_POLL),
            IterateResult::Success(_) => {},
        }
        match context.get_state() {
            ContextState::Ready => break,
            ContextState::Failed | ContextState::Terminated => {
                return Err(SoundServerError::ConnectFailed);
            },
            _ => {},
        }
    }
    Ok((mainloop, context))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_profiles_find_a2dp_sinks_by_prefix() {
        let profiles = CardProfiles {
            active: Some("off".to_string()),
            available: vec!["off".to_string(), "a2dp-sink-sbc".to_string()],
        };

        assert!(profiles.has_a2dp_sink());
        assert!(profiles.has("a2dp-sink-sbc"));
        assert!(!profiles.has("a2dp-sink"));
    }

    #[test]
    fn card_without_a2dp_has_no_a2dp_sink() {
        let profiles = CardProfiles {
            active: None,
            available: vec!["off".to_string(), "headset-head-unit".to_string()],
        };

        assert!(!profiles.has_a2dp_sink());
    }
}

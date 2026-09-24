//! The live side of the hi-res microphone section: the recorder and the
//! player of the microphone test, the media players it pauses, and the timer
//! that feeds `MicInput::Tick` while the model asks for it.

use {
    super::Controller,
    crate::{
        audio::{
            mic_test::{Player, Recorder},
            output,
        },
        ui::gtk::model::{
            Input, MicCapture, MicEffect, MicInput, MicSample, PlayerCommand, PlayerSample,
            RecorderSample, Recording,
        },
    },
    gtk::glib,
    std::{rc::Rc, time::Duration},
    tracing::{debug, error, warn},
};

/// The recorder and the player of the microphone test, and the tick timer.
/// The model decides when each starts and stops.
#[derive(Default)]
pub(super) struct MicDevices {
    recorder: Option<Recorder>,
    player: Option<Player>,
    /// The running tick timer and its interval.
    tick: Option<(Duration, glib::SourceId)>,
}

impl Controller {
    pub(super) fn run_mic(self: &Rc<Self>, effect: MicEffect) {
        match effect {
            MicEffect::SetHiRes { mac, enabled } => {
                self.with_aacp(mac, move |aacp, _| async move {
                    aacp.set_hires_mic_enabled(enabled).await;
                });
            },
            MicEffect::PauseMedia => {
                let task = self.backend.spawn_blocking(output::pause_media_players);
                let dispatch = self.dispatch.clone();
                glib::MainContext::default().spawn_local(async move {
                    let players = task.await.unwrap_or_else(|e| {
                        error!("Pausing the media players failed: {}", e);
                        Vec::new()
                    });
                    dispatch.send(Input::Microphone(MicInput::MediaPaused(players)));
                });
            },
            MicEffect::ResumeMedia(players) => {
                self.backend
                    .spawn_blocking(move || output::resume_media_players(&players));
            },
            MicEffect::StartRecording => {
                let mut devices = self.mic.borrow_mut();
                devices.player = None;
                devices.recorder = Some(Recorder::start());
            },
            MicEffect::StopRecording { take } => self.stop_recorder(take),
            MicEffect::Release => {
                let mut devices = self.mic.borrow_mut();
                devices.player = None;
                if let Some(recorder) = devices.recorder.take() {
                    // Joining the recorder thread waits, so it runs off the
                    // GTK thread.
                    self.backend.spawn_blocking(move || {
                        if let Err(e) = recorder.stop() {
                            debug!("Discarded microphone test recording: {}", e);
                        }
                    });
                }
            },
            MicEffect::LoadPlayer(Recording(pcm)) => {
                self.mic.borrow_mut().player = Some(Player::new(pcm));
                self.dispatch
                    .send(Input::Microphone(MicInput::Tick(self.mic_sample())));
            },
            MicEffect::Player(command) => {
                let devices = self.mic.borrow();
                let Some(player) = devices.player.as_ref() else {
                    warn!("No microphone test player for {:?}", command);
                    return;
                };
                match command {
                    PlayerCommand::Play => player.play(),
                    PlayerCommand::Pause => player.pause(),
                    PlayerCommand::Seek(to) => player.seek(to),
                }
            },
        }
    }

    /// Stop the recorder off the GTK thread and report the recording as
    /// `take`.
    fn stop_recorder(&self, take: u64) {
        let dispatch = self.dispatch.clone();
        let Some(recorder) = self.mic.borrow_mut().recorder.take() else {
            dispatch.send(Input::Microphone(MicInput::Recorded {
                take,
                result: Err("The recording was lost".to_string()),
            }));
            return;
        };
        let task = self
            .backend
            .spawn_blocking(move || recorder.stop().map(Recording).map_err(|e| e.to_string()));
        glib::MainContext::default().spawn_local(async move {
            let result = task.await.unwrap_or_else(|e| {
                error!("Stopping the microphone test recorder failed: {}", e);
                Err("The recorder crashed".to_string())
            });
            dispatch.send(Input::Microphone(MicInput::Recorded { take, result }));
        });
    }

    /// Start, change or stop the tick timer to match what the model asks for.
    pub(super) fn sync_mic_tick(self: &Rc<Self>) {
        let wanted = self.model.borrow().mic_tick();
        let mut devices = self.mic.borrow_mut();
        if devices.tick.as_ref().map(|(interval, _)| *interval) == wanted {
            return;
        }
        if let Some((_, source)) = devices.tick.take() {
            source.remove();
        }
        let Some(interval) = wanted else {
            return;
        };
        let this = Rc::downgrade(self);
        let source = glib::timeout_add_local(interval, move || {
            let Some(this) = this.upgrade() else {
                return glib::ControlFlow::Break;
            };
            this.dispatch
                .send(Input::Microphone(MicInput::Tick(this.mic_sample())));
            glib::ControlFlow::Continue
        });
        devices.tick = Some((interval, source));
    }

    /// Read the capture state of every connected device, the recorder and
    /// the player. Never waits: when the device list is being written, the
    /// capture state is left out.
    fn mic_sample(&self) -> MicSample {
        let capture = self.device_managers.try_read().ok().map(|managers| {
            managers
                .iter()
                .filter_map(|(mac, managers)| {
                    let aacp = managers.get_aacp()?;
                    Some((
                        mac.clone(),
                        MicCapture {
                            active: aacp.mic_active(),
                            level: aacp.mic_level(),
                            app: aacp.mic_app(),
                        },
                    ))
                })
                .collect()
        });
        let devices = self.mic.borrow();
        MicSample {
            capture,
            recorder: devices.recorder.as_ref().map(|r| RecorderSample {
                elapsed: r.elapsed(),
                finished: r.finished(),
            }),
            player: devices.player.as_ref().map(|p| PlayerSample {
                position: p.position(),
                duration: p.duration(),
                playing: p.is_playing(),
                error: p.error().map(|e| e.to_string()),
            }),
        }
    }
}

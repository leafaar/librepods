//! The hi-res microphone: whether an app is capturing from it, the microphone
//! test, and how often the view needs a `MicInput::Tick`.
//!
//! The recorder and the player live in the app, which runs their threads.
//! The model only holds the phase of the test and what the last tick read
//! from them, and decides what the app does with them next.

use {
    super::{Effect, Model},
    crate::{
        audio::mic_test::{MAX_RECORDING, SKIP},
        ui::format::mmss,
    },
    std::{collections::HashMap, fmt, mem, time::Duration},
};

/// Tick rate while something on screen moves: the recording timer, the
/// playback position or the level meter.
pub(crate) const MIC_TICK: Duration = Duration::from_millis(50);
/// Tick rate while only watching for an app to open the hi-res microphone.
/// Nothing reports that, so it is polled slowly to bring up the level meter.
pub(crate) const MIC_WATCH: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub(crate) enum MicInput {
    /// The hi-res microphone switch on the page of the AirPods at the address.
    SetHiRes(String, bool),
    Record,
    Stop,
    Play,
    Pause,
    Skip {
        forward: bool,
    },
    Seek(Duration),
    Done,
    /// The media players that were playing were paused; these are the ones
    /// to resume when the test ends.
    MediaPaused(Vec<String>),
    /// The recorder stopped. `take` is the one `MicEffect::StopRecording`
    /// carried.
    Recorded {
        take: u64,
        result: Result<Recording, String>,
    },
    /// What the app read from the devices, the recorder and the player.
    Tick(MicSample),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MicEffect {
    SetHiRes {
        mac: String,
        enabled: bool,
    },
    /// Pause the playing media players off the GTK thread and come back with
    /// `MicInput::MediaPaused`.
    PauseMedia,
    ResumeMedia(Vec<String>),
    /// Drop the player and start the recorder.
    StartRecording,
    /// Stop the recorder off the GTK thread and come back with
    /// `MicInput::Recorded` carrying `take`.
    StopRecording {
        take: u64,
    },
    /// Stop the recorder, if one runs, throwing its recording away, and drop
    /// the player.
    Release,
    /// Build the player for this recording, then send a tick so its length
    /// shows at once.
    LoadPlayer(Recording),
    Player(PlayerCommand),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum PlayerCommand {
    Play,
    Pause,
    Seek(Duration),
}

/// PCM from the recorder. Debug prints only its size, since inputs and
/// effects are logged and a recording is up to about 38 MB.
#[derive(Clone, PartialEq)]
pub(crate) struct Recording(pub(crate) Vec<u8>);

impl fmt::Debug for Recording {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Recording({} bytes)", self.0.len())
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MicSample {
    /// Capture state per connected AirPods, None when the device list could
    /// not be read without waiting; the last known state is kept then.
    pub(crate) capture: Option<Vec<(String, MicCapture)>>,
    pub(crate) recorder: Option<RecorderSample>,
    pub(crate) player: Option<PlayerSample>,
}

/// Whether an app records from the hi-res microphone of one device.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct MicCapture {
    pub(crate) active: bool,
    /// Input level from 0 to 1.
    pub(crate) level: f32,
    pub(crate) app: Option<String>,
}

impl MicCapture {
    /// The line above the level meter.
    pub(crate) fn title(&self) -> String {
        match &self.app {
            Some(app) => format!("In use by {app}"),
            None => "Input level".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RecorderSample {
    pub(crate) elapsed: Duration,
    /// The recorder stopped by itself: it reached MAX_RECORDING or failed.
    pub(crate) finished: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlayerSample {
    pub(crate) position: Duration,
    pub(crate) duration: Duration,
    pub(crate) playing: bool,
    pub(crate) error: Option<String>,
}

/// The phase of the microphone test.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MicTest {
    Idle,
    /// Waiting for the playing media to pause before recording.
    Starting,
    Recording {
        elapsed: Duration,
    },
    /// The recorder was told to stop and its thread is being joined.
    Stopping,
    Ready(Playback),
    Failed(String),
}

impl MicTest {
    /// A test is under way and the media players stay paused.
    fn is_running(&self) -> bool {
        matches!(
            self,
            MicTest::Starting | MicTest::Recording { .. } | MicTest::Stopping | MicTest::Ready(_)
        )
    }
}

/// The player of a finished recording, as of the last tick.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Playback {
    pub(crate) position: Duration,
    pub(crate) duration: Duration,
    pub(crate) playing: bool,
    /// Play was pressed and the player has not started yet. It waits for the
    /// sound server after a recording, so this keeps the ticks coming.
    pub(crate) play_requested: bool,
    pub(crate) error: Option<String>,
}

/// What the microphone test rows show.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MicTestView {
    pub(crate) status: String,
    /// Label of the record button, None to hide it.
    pub(crate) record: Option<&'static str>,
    pub(crate) stop: bool,
    /// The player controls and the Done button, shown once a take is ready.
    pub(crate) playback: Option<PlaybackView>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlaybackView {
    pub(crate) position: Duration,
    pub(crate) duration: Duration,
    /// "0:03 / 0:10"
    pub(crate) label: String,
    /// Show Pause instead of Play.
    pub(crate) playing: bool,
    /// Label of the skip buttons, "-5s" and "+5s".
    pub(crate) back: String,
    pub(crate) forward: String,
}

const TEST_HINT: &str = "Record yourself, then play it back to hear what apps receive. Music is \
                         paused until you press Done.";
const READY_HINT: &str = "Music stays paused until you press Done.";

impl MicTest {
    pub(crate) fn view(&self) -> MicTestView {
        let status = |status: String| MicTestView {
            status,
            record: None,
            stop: false,
            playback: None,
        };
        match self {
            MicTest::Idle => MicTestView {
                record: Some("Record"),
                ..status(TEST_HINT.to_string())
            },
            MicTest::Failed(e) => MicTestView {
                record: Some("Record"),
                ..status(e.clone())
            },
            MicTest::Starting => status("Pausing media…".to_string()),
            MicTest::Recording { elapsed } => MicTestView {
                stop: true,
                ..status(format!(
                    "Recording {} (max {})",
                    mmss(*elapsed),
                    mmss(MAX_RECORDING)
                ))
            },
            MicTest::Stopping => status("Finishing the recording…".to_string()),
            MicTest::Ready(playback) => {
                let position = playback.position.min(playback.duration);
                let skip = SKIP.as_secs();
                MicTestView {
                    record: Some("Record Again"),
                    playback: Some(PlaybackView {
                        position,
                        duration: playback.duration,
                        label: format!("{} / {}", mmss(position), mmss(playback.duration)),
                        playing: playback.playing || playback.play_requested,
                        back: format!("-{skip}s"),
                        forward: format!("+{skip}s"),
                    }),
                    ..status(
                        playback
                            .error
                            .clone()
                            .unwrap_or_else(|| READY_HINT.to_string()),
                    )
                }
            },
        }
    }
}

/// The microphone part of the model.
pub(super) struct MicState {
    test: MicTest,
    /// Media players paused for the test, resumed when it ends.
    paused: Vec<String>,
    /// Bumped on every recorder stop and whenever the test ends, so a stop
    /// result that arrives late is dropped instead of reviving the test.
    take: u64,
    /// Capture state per connected AirPods, from the last tick.
    capture: HashMap<String, MicCapture>,
    /// Ticks for the level meter and the player only matter while the
    /// window shows.
    window_visible: bool,
}

impl Default for MicState {
    fn default() -> Self {
        Self {
            test: MicTest::Idle,
            paused: Vec::new(),
            take: 0,
            capture: HashMap::new(),
            window_visible: false,
        }
    }
}

fn effects(mic: impl IntoIterator<Item = MicEffect>) -> Vec<Effect> {
    mic.into_iter().map(Effect::Microphone).collect()
}

impl Model {
    pub(super) fn microphone(&mut self, input: MicInput) -> Vec<Effect> {
        match input {
            MicInput::SetHiRes(mac, enabled) => self.set_hires_mic(mac, enabled),
            MicInput::Record => self.record(),
            MicInput::Stop => self.stop_recording(),
            MicInput::Play => self.playback(|p| {
                p.play_requested = true;
                Some(PlayerCommand::Play)
            }),
            MicInput::Pause => self.playback(|p| {
                p.playing = false;
                p.play_requested = false;
                Some(PlayerCommand::Pause)
            }),
            MicInput::Skip { forward } => self.playback(|p| {
                let to = if forward {
                    p.position.saturating_add(SKIP).min(p.duration)
                } else {
                    p.position.saturating_sub(SKIP)
                };
                p.position = to;
                Some(PlayerCommand::Seek(to))
            }),
            MicInput::Seek(to) => self.playback(|p| {
                let to = to.min(p.duration);
                // An unchanged position is the scale settling, not a seek.
                (to != p.position).then(|| {
                    p.position = to;
                    PlayerCommand::Seek(to)
                })
            }),
            MicInput::Done => self.end_mic_test(),
            MicInput::MediaPaused(players) => self.media_paused(players),
            MicInput::Recorded { take, result } => self.recorded(take, result),
            MicInput::Tick(sample) => self.mic_tick_sample(sample),
        }
    }

    fn set_hires_mic(&mut self, mac: String, enabled: bool) -> Vec<Effect> {
        if !self.airpods.contains_key(&mac) {
            return Vec::new();
        }
        self.settings.hires_mic_enabled = enabled;
        let mut out = vec![
            Effect::Microphone(MicEffect::SetHiRes { mac, enabled }),
            Effect::SaveSettings,
        ];
        // The test records from the hi-res source, which is going away.
        if !enabled {
            out.extend(self.end_mic_test());
        }
        out
    }

    fn record(&mut self) -> Vec<Effect> {
        let mic = &mut self.mic;
        if !self.settings.hires_mic_enabled
            || self.airpods.is_empty()
            || matches!(
                mic.test,
                MicTest::Starting | MicTest::Recording { .. } | MicTest::Stopping
            )
        {
            return Vec::new();
        }
        // Players already paused by an earlier take stay paused and listed.
        if !mic.paused.is_empty() {
            mic.test = MicTest::Recording {
                elapsed: Duration::ZERO,
            };
            return effects([MicEffect::StartRecording]);
        }
        // Music would play over the recording and the playback, so it is
        // paused for the whole test.
        let replaying = matches!(mic.test, MicTest::Ready(_));
        mic.test = MicTest::Starting;
        let release = replaying.then_some(MicEffect::Release);
        effects(release.into_iter().chain([MicEffect::PauseMedia]))
    }

    fn media_paused(&mut self, players: Vec<String>) -> Vec<Effect> {
        let mic = &mut self.mic;
        if matches!(mic.test, MicTest::Starting) {
            mic.paused = players;
            mic.test = MicTest::Recording {
                elapsed: Duration::ZERO,
            };
            effects([MicEffect::StartRecording])
        } else if mic.test.is_running() {
            // A pause from a test that ended while a new one started.
            mic.paused.extend(players);
            Vec::new()
        } else if players.is_empty() {
            Vec::new()
        } else {
            // The test ended while the players were being paused.
            effects([MicEffect::ResumeMedia(players)])
        }
    }

    fn stop_recording(&mut self) -> Vec<Effect> {
        let mic = &mut self.mic;
        if !matches!(mic.test, MicTest::Recording { .. }) {
            return Vec::new();
        }
        mic.test = MicTest::Stopping;
        mic.take += 1;
        effects([MicEffect::StopRecording { take: mic.take }])
    }

    fn recorded(&mut self, take: u64, result: Result<Recording, String>) -> Vec<Effect> {
        let mic = &mut self.mic;
        if take != mic.take || !matches!(mic.test, MicTest::Stopping) {
            return Vec::new();
        }
        match result {
            Ok(recording) => {
                mic.test = MicTest::Ready(Playback::default());
                effects([MicEffect::LoadPlayer(recording)])
            },
            Err(e) => {
                // A failed test has no Done button, so the music comes back now.
                mic.test = MicTest::Failed(e);
                let paused = mem::take(&mut mic.paused);
                effects((!paused.is_empty()).then_some(MicEffect::ResumeMedia(paused)))
            },
        }
    }

    /// Change the ready player and send the command `change` returns.
    fn playback(
        &mut self,
        change: impl FnOnce(&mut Playback) -> Option<PlayerCommand>,
    ) -> Vec<Effect> {
        let MicTest::Ready(playback) = &mut self.mic.test else {
            return Vec::new();
        };
        effects(change(playback).map(MicEffect::Player))
    }

    fn mic_tick_sample(&mut self, sample: MicSample) -> Vec<Effect> {
        if let Some(capture) = sample.capture {
            self.mic.capture = capture
                .into_iter()
                .filter(|(mac, _)| self.airpods.contains_key(mac))
                .map(|(mac, mut capture)| {
                    capture.level = if capture.level.is_finite() {
                        capture.level.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    (mac, capture)
                })
                .collect();
        }
        match (&mut self.mic.test, sample.recorder, sample.player) {
            (MicTest::Recording { elapsed }, Some(recorder), _) => {
                *elapsed = recorder.elapsed;
                if recorder.finished {
                    return self.stop_recording();
                }
            },
            (MicTest::Ready(playback), _, Some(player)) => {
                if player.playing || player.error.is_some() {
                    playback.play_requested = false;
                }
                playback.position = player.position.min(player.duration);
                playback.duration = player.duration;
                playback.playing = player.playing;
                playback.error = player.error;
            },
            _ => {},
        }
        Vec::new()
    }

    /// End the test from any phase: stop the recorder, drop the player and
    /// resume the media players it paused.
    pub(super) fn end_mic_test(&mut self) -> Vec<Effect> {
        let mic = &mut self.mic;
        mic.take += 1;
        let was = mem::replace(&mut mic.test, MicTest::Idle);
        let release = matches!(was, MicTest::Recording { .. } | MicTest::Ready(_))
            .then_some(MicEffect::Release);
        let paused = mem::take(&mut mic.paused);
        let resume = (!paused.is_empty()).then_some(MicEffect::ResumeMedia(paused));
        effects(release.into_iter().chain(resume))
    }

    /// The AirPods at `mac` went away. The test records from them, and its
    /// Done button goes away with them.
    pub(super) fn mic_disconnected(&mut self, mac: &str, was_airpods: bool) -> Vec<Effect> {
        self.mic.capture.remove(mac);
        if was_airpods {
            self.end_mic_test()
        } else {
            Vec::new()
        }
    }

    pub(super) fn set_window_visible(&mut self, visible: bool) {
        self.mic.window_visible = visible;
    }

    // Read access for the view and the app.

    pub(crate) fn mic_test(&self) -> &MicTest {
        &self.mic.test
    }

    /// The capture on the AirPods at `mac`, while an app records from them.
    pub(crate) fn mic_capture(&self, mac: &str) -> Option<&MicCapture> {
        self.mic.capture.get(mac).filter(|c| c.active)
    }

    /// The conversation awareness switch is locked while a capture turns
    /// conversation awareness off, so the user cannot fight the override.
    pub(crate) fn conversation_awareness_locked(&self, mac: &str) -> bool {
        self.settings.hires_mic_pause_convo && self.mic_capture(mac).is_some()
    }

    /// How often the app should send `MicInput::Tick`, None for no ticks.
    pub(crate) fn mic_tick(&self) -> Option<Duration> {
        // Also with the window hidden, to notice the recorder reaching
        // MAX_RECORDING.
        if matches!(self.mic.test, MicTest::Recording { .. }) {
            return Some(MIC_TICK);
        }
        if !self.mic.window_visible {
            return None;
        }
        if matches!(&self.mic.test, MicTest::Ready(p) if p.playing || p.play_requested)
            || self
                .airpods
                .keys()
                .any(|mac| self.mic_capture(mac).is_some())
        {
            return Some(MIC_TICK);
        }
        (self.settings.hires_mic_enabled && !self.airpods.is_empty()).then_some(MIC_WATCH)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::ui::{
            gtk::model::{
                AirPodsSnapshot, Input,
                tests::{PODS, connected, model},
            },
            messages::BluetoothUIMessage,
        },
    };

    fn mic(model: &mut Model, input: MicInput) -> Vec<Effect> {
        model.update(Input::Microphone(input))
    }

    fn only(effect: MicEffect) -> Vec<Effect> {
        vec![Effect::Microphone(effect)]
    }

    fn visible_with_airpods() -> Model {
        let mut model = model();
        connected(&mut model, AirPodsSnapshot::default());
        model.update(Input::WindowVisible(true));
        model
    }

    fn recorder(elapsed: u64, finished: bool) -> MicSample {
        MicSample {
            recorder: Some(RecorderSample {
                elapsed: Duration::from_secs(elapsed),
                finished,
            }),
            ..MicSample::default()
        }
    }

    fn player(position: u64, duration: u64, playing: bool, error: Option<&str>) -> MicSample {
        MicSample {
            player: Some(PlayerSample {
                position: Duration::from_secs(position),
                duration: Duration::from_secs(duration),
                playing,
                error: error.map(str::to_string),
            }),
            ..MicSample::default()
        }
    }

    fn capture(active: bool, level: f32, app: Option<&str>) -> MicSample {
        MicSample {
            capture: Some(vec![(
                PODS.to_string(),
                MicCapture {
                    active,
                    level,
                    app: app.map(str::to_string),
                },
            )]),
            ..MicSample::default()
        }
    }

    /// Record with `players` playing and stop; returns the take to answer.
    fn recorded_take(model: &mut Model, players: &[&str]) -> u64 {
        assert_eq!(mic(model, MicInput::Record), only(MicEffect::PauseMedia));
        let players = players.iter().map(ToString::to_string).collect();
        assert_eq!(
            mic(model, MicInput::MediaPaused(players)),
            only(MicEffect::StartRecording)
        );
        let effects = mic(model, MicInput::Stop);
        let [Effect::Microphone(MicEffect::StopRecording { take })] = effects.as_slice() else {
            panic!("stop expected, got {effects:?}");
        };
        *take
    }

    fn ready(model: &mut Model, players: &[&str]) {
        let take = recorded_take(model, players);
        let recording = Recording(vec![1, 2]);
        assert_eq!(
            mic(
                model,
                MicInput::Recorded {
                    take,
                    result: Ok(recording.clone())
                }
            ),
            only(MicEffect::LoadPlayer(recording))
        );
        mic(model, MicInput::Tick(player(0, 10, false, None)));
    }

    #[test]
    fn recording_starts_after_the_media_paused() {
        let mut model = visible_with_airpods();
        assert_eq!(
            mic(&mut model, MicInput::Record),
            only(MicEffect::PauseMedia)
        );
        assert_eq!(model.mic_test(), &MicTest::Starting);
        assert_eq!(model.mic_test().view().status, "Pausing media…");
        // A second press while starting does nothing.
        assert!(mic(&mut model, MicInput::Record).is_empty());

        let effects = mic(&mut model, MicInput::MediaPaused(vec!["spotify".into()]));
        assert_eq!(effects, only(MicEffect::StartRecording));
        mic(&mut model, MicInput::Tick(recorder(65, false)));
        let view = model.mic_test().view();
        assert_eq!(view.status, "Recording 1:05 (max 5:00)");
        assert!(view.stop);
        assert_eq!(view.record, None);
    }

    #[test]
    fn record_needs_the_hires_mic_and_connected_airpods() {
        let mut model = model();
        assert!(mic(&mut model, MicInput::Record).is_empty());
        connected(&mut model, AirPodsSnapshot::default());
        mic(&mut model, MicInput::SetHiRes(PODS.to_string(), false));
        assert!(mic(&mut model, MicInput::Record).is_empty());
        assert_eq!(model.mic_test(), &MicTest::Idle);
    }

    #[test]
    fn stop_then_ready_plays_and_done_resumes_the_media() {
        let mut model = visible_with_airpods();
        ready(&mut model, &["spotify"]);
        let view = model.mic_test().view();
        assert_eq!(view.record, Some("Record Again"));
        let playback = view.playback.unwrap();
        assert_eq!(playback.label, "0:00 / 0:10");
        assert!(!playback.playing);

        assert_eq!(
            mic(&mut model, MicInput::Play),
            only(MicEffect::Player(PlayerCommand::Play))
        );
        assert!(model.mic_test().view().playback.unwrap().playing);
        assert_eq!(
            mic(&mut model, MicInput::Skip { forward: true }),
            only(MicEffect::Player(PlayerCommand::Seek(Duration::from_secs(
                5
            ))))
        );
        mic(&mut model, MicInput::Skip { forward: true });
        assert_eq!(
            mic(&mut model, MicInput::Skip { forward: true }),
            only(MicEffect::Player(PlayerCommand::Seek(Duration::from_secs(
                10
            ))))
        );
        assert_eq!(
            mic(&mut model, MicInput::Seek(Duration::from_secs(2))),
            only(MicEffect::Player(PlayerCommand::Seek(Duration::from_secs(
                2
            ))))
        );
        assert_eq!(
            mic(&mut model, MicInput::Skip { forward: false }),
            only(MicEffect::Player(PlayerCommand::Seek(Duration::ZERO)))
        );
        // The scale settling on the same value is not a seek.
        assert!(mic(&mut model, MicInput::Seek(Duration::ZERO)).is_empty());
        assert_eq!(
            mic(&mut model, MicInput::Pause),
            only(MicEffect::Player(PlayerCommand::Pause))
        );

        assert_eq!(
            mic(&mut model, MicInput::Done),
            vec![
                Effect::Microphone(MicEffect::Release),
                Effect::Microphone(MicEffect::ResumeMedia(vec!["spotify".into()])),
            ]
        );
        assert_eq!(model.mic_test(), &MicTest::Idle);
    }

    #[test]
    fn record_again_keeps_the_paused_players() {
        let mut model = visible_with_airpods();
        ready(&mut model, &["spotify"]);
        assert_eq!(
            mic(&mut model, MicInput::Record),
            only(MicEffect::StartRecording)
        );
        assert!(matches!(model.mic_test(), MicTest::Recording { .. }));
        mic(&mut model, MicInput::Stop);
        assert_eq!(model.mic_test(), &MicTest::Stopping);
        let effects = mic(&mut model, MicInput::Done);
        assert_eq!(
            effects,
            only(MicEffect::ResumeMedia(vec!["spotify".into()]))
        );
    }

    #[test]
    fn record_again_with_nothing_paused_releases_the_player_first() {
        let mut model = visible_with_airpods();
        ready(&mut model, &[]);
        assert_eq!(
            mic(&mut model, MicInput::Record),
            vec![
                Effect::Microphone(MicEffect::Release),
                Effect::Microphone(MicEffect::PauseMedia),
            ]
        );
    }

    #[test]
    fn failed_recording_resumes_the_players_at_once() {
        let mut model = visible_with_airpods();
        let take = recorded_take(&mut model, &["spotify"]);
        let effects = mic(
            &mut model,
            MicInput::Recorded {
                take,
                result: Err("No sound was recorded".into()),
            },
        );
        assert_eq!(
            effects,
            only(MicEffect::ResumeMedia(vec!["spotify".into()]))
        );
        let view = model.mic_test().view();
        assert_eq!(view.status, "No sound was recorded");
        assert_eq!(view.record, Some("Record"));
        assert!(view.playback.is_none());
        // Nothing left to resume when the test ends later.
        assert!(mic(&mut model, MicInput::Done).is_empty());
    }

    #[test]
    fn late_recording_results_are_ignored() {
        let mut model = visible_with_airpods();
        let take = recorded_take(&mut model, &[]);
        // Done while the recorder was being joined.
        mic(&mut model, MicInput::Done);
        assert!(
            mic(
                &mut model,
                MicInput::Recorded {
                    take,
                    result: Ok(Recording(vec![1]))
                }
            )
            .is_empty()
        );
        assert_eq!(model.mic_test(), &MicTest::Idle);

        // A new take ignores the result of the one before it.
        let second = recorded_take(&mut model, &[]);
        assert_ne!(second, take);
        assert!(
            mic(
                &mut model,
                MicInput::Recorded {
                    take,
                    result: Ok(Recording(vec![1]))
                }
            )
            .is_empty()
        );
        assert_eq!(model.mic_test(), &MicTest::Stopping);
    }

    #[test]
    fn media_paused_after_the_test_ended_is_resumed() {
        let mut model = visible_with_airpods();
        mic(&mut model, MicInput::Record);
        mic(&mut model, MicInput::Done);
        assert_eq!(
            mic(&mut model, MicInput::MediaPaused(vec!["vlc".into()])),
            only(MicEffect::ResumeMedia(vec!["vlc".into()]))
        );
        assert!(mic(&mut model, MicInput::MediaPaused(Vec::new())).is_empty());
    }

    #[test]
    fn recorder_reaching_its_limit_stops_the_recording() {
        let mut model = visible_with_airpods();
        mic(&mut model, MicInput::Record);
        mic(&mut model, MicInput::MediaPaused(Vec::new()));
        assert!(mic(&mut model, MicInput::Tick(recorder(1, false))).is_empty());
        let effects = mic(&mut model, MicInput::Tick(recorder(300, true)));
        assert!(matches!(
            effects.as_slice(),
            [Effect::Microphone(MicEffect::StopRecording { .. })]
        ));
        assert_eq!(model.mic_test(), &MicTest::Stopping);
    }

    #[test]
    fn disconnect_ends_the_test() {
        let mut model = visible_with_airpods();
        mic(&mut model, MicInput::Record);
        mic(&mut model, MicInput::MediaPaused(vec!["spotify".into()]));
        let effects = model.update(Input::Backend(BluetoothUIMessage::DeviceDisconnected(
            PODS.to_string(),
        )));
        assert_eq!(
            effects,
            vec![
                Effect::Microphone(MicEffect::Release),
                Effect::Microphone(MicEffect::ResumeMedia(vec!["spotify".into()])),
            ]
        );
        assert_eq!(model.mic_test(), &MicTest::Idle);
        assert_eq!(model.mic_tick(), None);
    }

    #[test]
    fn turning_the_hires_mic_off_ends_the_test_and_saves() {
        let mut model = visible_with_airpods();
        ready(&mut model, &["spotify"]);
        let effects = mic(&mut model, MicInput::SetHiRes(PODS.to_string(), false));
        assert_eq!(
            effects,
            vec![
                Effect::Microphone(MicEffect::SetHiRes {
                    mac: PODS.to_string(),
                    enabled: false
                }),
                Effect::SaveSettings,
                Effect::Microphone(MicEffect::Release),
                Effect::Microphone(MicEffect::ResumeMedia(vec!["spotify".into()])),
            ]
        );
        assert!(!model.settings().hires_mic_enabled);
        assert_eq!(model.mic_test(), &MicTest::Idle);
    }

    #[test]
    fn hires_switch_for_disconnected_airpods_does_nothing() {
        let mut model = model();
        assert!(mic(&mut model, MicInput::SetHiRes(PODS.to_string(), false)).is_empty());
        assert!(model.settings().hires_mic_enabled);
    }

    #[test]
    fn player_ticks_update_the_position_and_clear_the_request() {
        let mut model = visible_with_airpods();
        ready(&mut model, &[]);
        mic(&mut model, MicInput::Play);
        // The player waits for the sound server; the request keeps ticking.
        mic(&mut model, MicInput::Tick(player(0, 10, false, None)));
        assert_eq!(model.mic_tick(), Some(MIC_TICK));
        mic(&mut model, MicInput::Tick(player(3, 10, true, None)));
        assert_eq!(
            model.mic_test().view().playback.unwrap().label,
            "0:03 / 0:10"
        );
        // Played to the end.
        mic(&mut model, MicInput::Tick(player(10, 10, false, None)));
        assert_eq!(model.mic_tick(), Some(MIC_WATCH));

        mic(&mut model, MicInput::Play);
        mic(
            &mut model,
            MicInput::Tick(player(0, 10, false, Some("Playback failed"))),
        );
        assert_eq!(model.mic_test().view().status, "Playback failed");
        assert_eq!(model.mic_tick(), Some(MIC_WATCH));
    }

    #[test]
    fn tick_rate_follows_what_moves() {
        let mut model = model();
        model.update(Input::WindowVisible(true));
        assert_eq!(model.mic_tick(), None, "no AirPods, no ticks");

        connected(&mut model, AirPodsSnapshot::default());
        assert_eq!(model.mic_tick(), Some(MIC_WATCH));

        mic(&mut model, MicInput::Tick(capture(true, 0.5, Some("Zoom"))));
        assert_eq!(model.mic_tick(), Some(MIC_TICK));
        mic(&mut model, MicInput::Tick(capture(false, 0.0, None)));
        assert_eq!(model.mic_tick(), Some(MIC_WATCH));

        mic(&mut model, MicInput::Record);
        assert_eq!(model.mic_tick(), Some(MIC_WATCH), "waiting for the pause");
        mic(&mut model, MicInput::MediaPaused(Vec::new()));
        assert_eq!(model.mic_tick(), Some(MIC_TICK));
        // The recorder is watched with the window hidden too.
        model.update(Input::WindowVisible(false));
        assert_eq!(model.mic_tick(), Some(MIC_TICK));
        mic(&mut model, MicInput::Stop);
        assert_eq!(model.mic_tick(), None);

        model.update(Input::WindowVisible(true));
        mic(&mut model, MicInput::SetHiRes(PODS.to_string(), false));
        assert_eq!(model.mic_tick(), None, "idle with the hi-res mic off");
    }

    #[test]
    fn capture_shows_the_meter_and_locks_conversation_awareness() {
        let mut model = visible_with_airpods();
        assert!(model.mic_capture(PODS).is_none());
        mic(&mut model, MicInput::Tick(capture(true, 1.7, Some("Zoom"))));
        let shown = model.mic_capture(PODS).unwrap();
        assert_eq!(shown.title(), "In use by Zoom");
        assert!((shown.level - 1.0).abs() < f32::EPSILON);
        assert!(model.conversation_awareness_locked(PODS));
        assert!(
            model
                .update(Input::SetConversationAwareness(PODS.to_string(), true))
                .is_empty()
        );

        // A capture state that could not be read keeps the last one.
        mic(&mut model, MicInput::Tick(MicSample::default()));
        assert!(model.mic_capture(PODS).is_some());

        mic(&mut model, MicInput::Tick(capture(true, f32::NAN, None)));
        let shown = model.mic_capture(PODS).unwrap();
        assert_eq!(shown.title(), "Input level");
        assert!(shown.level.abs() < f32::EPSILON);

        model.update(Input::Setting(
            super::super::SettingChange::HiResMicPauseConvo(false),
        ));
        assert!(!model.conversation_awareness_locked(PODS));
    }

    #[test]
    fn capture_of_unknown_devices_is_dropped() {
        let mut model = model();
        mic(&mut model, MicInput::Tick(capture(true, 0.5, None)));
        assert!(model.mic_capture(PODS).is_none());
    }

    #[test]
    fn recording_debug_prints_its_size() {
        assert_eq!(format!("{:?}", Recording(vec![0; 4])), "Recording(4 bytes)");
    }
}

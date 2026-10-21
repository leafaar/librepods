//! A one-line summary of the AirPods' audio every `INTERVAL` while they are
//! connected, at debug, or at info when the problems it finds change. It
//! reads only what the app already tracks (the BlueZ transport, the AACP
//! state, the playback listener's last poll, the microphone stream counters),
//! so it costs no extra calls to the sound server or BlueZ.

use {
    crate::{
        bluetooth::{
            aacp::{AACPManager, AudioSource, EarDetectionStatus, SduCounts},
            bluez_watch::{Codec, TransportState, TransportStates},
        },
        media_controller::MediaController,
    },
    std::{
        fmt,
        time::{Duration, Instant},
    },
    tracing::{debug, info},
};

const INTERVAL: Duration = Duration::from_secs(10);
/// A transport that stays out of "active" this long while audio should flow
/// is reported. Starting a stream, and the A2DP reset that follows a
/// microphone start, take a few seconds of "idle" or "pending" on their own.
const OUTPUT_GRACE: Duration = Duration::from_secs(5);

/// The A2DP transport as the summary shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TransportView {
    /// None until BlueZ reported a state for this transport.
    state: Option<TransportState>,
    codec: Option<Codec>,
    /// How long the transport has been in `state`.
    age: Duration,
}

/// What the summary reports, read at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Health {
    /// None when BlueZ has no A2DP transport for the AirPods.
    transport: Option<TransportView>,
    /// The AirPods' OwnsConnection: whether they route their audio here.
    owns: bool,
    audio_source: Option<AudioSource>,
    /// Length of the AirPods' last Connected Devices report.
    connected_devices: usize,
    buds_in_ear: usize,
    buds: usize,
    /// A local player was playing at the playback listener's last poll.
    playing: bool,
    mic_enabled: bool,
    /// The app recording from the hi-res microphone, while it captures.
    mic_capture: Option<String>,
    /// Microphone stream SDUs since the previous summary.
    sdus: SduCounts,
}

/// Something in the summary that should not be.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Problem {
    /// Audio should reach the AirPods but BlueZ has no streaming transport.
    OutputNotStreaming {
        why: &'static str,
        transport: Option<(TransportState, Duration)>,
    },
    /// The microphone captures but the AirPods sent no audio.
    MicSilent,
    /// The decoder fell behind and SDUs were thrown away.
    SdusDropped(u64),
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputNotStreaming {
                why,
                transport: Some((state, age)),
            } => write!(
                f,
                "{why} but the A2DP transport is {state} for {}s",
                age.as_secs()
            ),
            Self::OutputNotStreaming {
                why,
                transport: None,
            } => write!(f, "{why} but there is no A2DP transport"),
            Self::MicSilent => f.write_str("the microphone captures but no audio arrived"),
            Self::SdusDropped(n) => write!(f, "{n} microphone SDUs dropped"),
        }
    }
}

impl Problem {
    fn kind(&self) -> ProblemKind {
        match self {
            Self::OutputNotStreaming { why, .. } => ProblemKind::OutputNotStreaming(why),
            Self::MicSilent => ProblemKind::MicSilent,
            Self::SdusDropped(_) => ProblemKind::SdusDropped,
        }
    }
}

/// A problem without the numbers that move every summary (how long, how
/// many), so a problem that persists is the same kind from one summary to the
/// next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProblemKind {
    OutputNotStreaming(&'static str),
    MicSilent,
    SdusDropped,
}

/// How the problems of one summary compare with the previous summary's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Same,
    /// Problems appeared, went or were replaced by others, and some remain.
    Changed,
    /// The previous summary had problems and this one has none.
    Cleared,
}

/// Remembers the problem kinds of the last summary, so a problem is logged at
/// info when it appears or clears and not again every INTERVAL while it lasts.
#[derive(Debug, Default)]
struct ProblemLog {
    last: Vec<ProblemKind>,
}

impl ProblemLog {
    fn update(&mut self, problems: &[Problem]) -> Change {
        let kinds: Vec<ProblemKind> = problems.iter().map(Problem::kind).collect();
        if kinds == self.last {
            return Change::Same;
        }
        self.last = kinds;
        if self.last.is_empty() {
            Change::Cleared
        } else {
            Change::Changed
        }
    }
}

impl Health {
    fn problems(&self) -> Vec<Problem> {
        let mut problems = Vec::new();
        // Local media only goes to the AirPods while they route audio here;
        // otherwise it plays on the speakers and no transport is expected.
        let output_needed = if self.mic_capture.is_some() {
            Some("the microphone captures")
        } else if self.playing && self.owns {
            Some("media plays")
        } else {
            None
        };
        if let Some(why) = output_needed {
            match &self.transport {
                None => problems.push(Problem::OutputNotStreaming {
                    why,
                    transport: None,
                }),
                Some(TransportView {
                    state: Some(state),
                    age,
                    ..
                }) if !state.is_active() && *age >= OUTPUT_GRACE => {
                    problems.push(Problem::OutputNotStreaming {
                        why,
                        transport: Some((state.clone(), *age)),
                    });
                },
                // Streaming, still within the grace, or a state BlueZ never
                // reported: an unknown state may well be streaming, so only
                // a known one is held against the transport.
                Some(_) => {},
            }
        }
        if self.mic_capture.is_some() && self.sdus.received == 0 {
            problems.push(Problem::MicSilent);
        }
        if self.sdus.dropped > 0 {
            problems.push(Problem::SdusDropped(self.sdus.dropped));
        }
        problems
    }
}

impl fmt::Display for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let yes_no = |b: bool| if b { "yes" } else { "no" };
        match &self.transport {
            Some(t) => {
                match &t.state {
                    Some(state) => write!(f, "transport={state}")?,
                    None => f.write_str("transport=unknown")?,
                }
                if let Some(codec) = t.codec {
                    write!(f, " {codec}")?;
                }
                write!(f, " for {}s", t.age.as_secs())?;
            },
            None => f.write_str("transport=none")?,
        }
        write!(f, " owns={}", yes_no(self.owns))?;
        match &self.audio_source {
            Some(source) => write!(f, " source={source}")?,
            None => f.write_str(" source=unknown")?,
        }
        write!(
            f,
            " devices={} in_ear={}/{} playing={}",
            self.connected_devices,
            self.buds_in_ear,
            self.buds,
            yes_no(self.playing)
        )?;
        match (&self.mic_capture, self.mic_enabled) {
            (Some(app), _) => write!(
                f,
                " mic=capturing for {app}, {} SDUs, {} dropped",
                self.sdus.received, self.sdus.dropped
            ),
            (None, true) => f.write_str(" mic=idle"),
            (None, false) => f.write_str(" mic=off"),
        }
    }
}

/// Log the summary for the AirPods `mac` every INTERVAL, until the task is
/// aborted with the connection.
pub(crate) async fn run(
    mac: String,
    aacp: AACPManager,
    media: MediaController,
    transports: TransportStates,
) {
    let mut last = aacp.sdu_counts();
    let mut log = ProblemLog::default();
    loop {
        tokio::time::sleep(INTERVAL).await;
        let counts = aacp.sdu_counts();
        let health = read(&mac, &aacp, &media, &transports, counts.since(last)).await;
        last = counts;
        let problems = health.problems();
        let change = log.update(&problems);
        let problems: Vec<String> = problems.iter().map(ToString::to_string).collect();
        match change {
            Change::Cleared => info!("[health] {mac}: {health}; ok again"),
            Change::Changed => info!("[health] {mac}: {health}; {}", problems.join("; ")),
            Change::Same if problems.is_empty() => debug!("[health] {mac}: {health}"),
            Change::Same => debug!("[health] {mac}: {health}; {}", problems.join("; ")),
        }
    }
}

async fn read(
    mac: &str,
    aacp: &AACPManager,
    media: &MediaController,
    transports: &TransportStates,
    sdus: SduCounts,
) -> Health {
    let now = Instant::now();
    let transport = transports.get(mac).map(|t| TransportView {
        state: t.state,
        codec: t.codec,
        age: now.saturating_duration_since(t.since),
    });
    let playing = media.is_playing().await;
    let state = aacp.state.lock().await;
    Health {
        transport,
        owns: state.owns,
        audio_source: state.audio_source.clone(),
        connected_devices: state.connected_devices.len(),
        buds_in_ear: state
            .ear_detection_status
            .iter()
            .filter(|s| **s == EarDetectionStatus::InEar)
            .count(),
        buds: state.ear_detection_status.len(),
        playing,
        mic_enabled: aacp.hires_mic_enabled(),
        mic_capture: aacp
            .mic_active()
            .then(|| aacp.mic_app().unwrap_or_else(|| "an app".to_string())),
        sdus,
    }
}

#[cfg(test)]
mod tests {
    use {super::*, crate::bluetooth::aacp::AudioSourceType};

    fn transport(state: TransportState, secs: u64) -> TransportView {
        TransportView {
            state: Some(state),
            codec: Some(Codec(0x02)),
            age: Duration::from_secs(secs),
        }
    }

    fn healthy() -> Health {
        Health {
            transport: Some(transport(TransportState::Active, 42)),
            owns: true,
            audio_source: Some(AudioSource {
                mac: "11:22:33:44:55:66".to_string(),
                r#type: AudioSourceType::Media,
            }),
            connected_devices: 1,
            buds_in_ear: 2,
            buds: 2,
            playing: true,
            mic_enabled: true,
            mic_capture: None,
            sdus: SduCounts::default(),
        }
    }

    #[test]
    fn summary_reads_as_one_line() {
        assert_eq!(
            healthy().to_string(),
            "transport=active AAC for 42s owns=yes source=11:22:33:44:55:66 (Media) devices=1 \
             in_ear=2/2 playing=yes mic=idle"
        );
        let capturing = Health {
            transport: None,
            audio_source: None,
            mic_capture: Some("Discord".to_string()),
            sdus: SduCounts {
                received: 1000,
                dropped: 3,
            },
            ..healthy()
        };
        assert_eq!(
            capturing.to_string(),
            "transport=none owns=yes source=unknown devices=1 in_ear=2/2 playing=yes \
             mic=capturing for Discord, 1000 SDUs, 3 dropped"
        );
    }

    #[test]
    fn streaming_audio_is_healthy() {
        assert_eq!(healthy().problems(), []);
        let capturing = Health {
            mic_capture: Some("Discord".to_string()),
            sdus: SduCounts {
                received: 1000,
                dropped: 0,
            },
            ..healthy()
        };
        assert_eq!(capturing.problems(), []);
    }

    #[test]
    fn idle_transport_while_media_plays_here_is_reported_after_the_grace() {
        let idle = |secs| Health {
            transport: Some(transport(TransportState::Idle, secs)),
            ..healthy()
        };
        assert_eq!(idle(OUTPUT_GRACE.as_secs() - 1).problems(), []);
        assert_eq!(
            idle(12).problems(),
            [Problem::OutputNotStreaming {
                why: "media plays",
                transport: Some((TransportState::Idle, Duration::from_secs(12))),
            }]
        );
        assert_eq!(
            idle(12).problems()[0].to_string(),
            "media plays but the A2DP transport is idle for 12s"
        );
    }

    #[test]
    fn media_playing_while_another_device_owns_the_audio_needs_no_transport() {
        let elsewhere = Health {
            transport: None,
            owns: false,
            ..healthy()
        };
        assert_eq!(elsewhere.problems(), []);
    }

    #[test]
    fn a_capture_without_output_or_audio_is_reported() {
        let silent = Health {
            transport: Some(transport(TransportState::Pending, 30)),
            playing: false,
            mic_capture: Some("Discord".to_string()),
            sdus: SduCounts {
                received: 0,
                dropped: 0,
            },
            ..healthy()
        };
        let problems: Vec<String> = silent.problems().iter().map(ToString::to_string).collect();
        assert_eq!(
            problems,
            [
                "the microphone captures but the A2DP transport is pending for 30s",
                "the microphone captures but no audio arrived",
            ]
        );
    }

    #[test]
    fn dropped_sdus_are_reported_with_the_missing_transport() {
        let dropped = Health {
            transport: None,
            sdus: SduCounts {
                received: 900,
                dropped: 7,
            },
            mic_capture: Some("Discord".to_string()),
            ..healthy()
        };
        let problems: Vec<String> = dropped.problems().iter().map(ToString::to_string).collect();
        assert_eq!(
            problems,
            [
                "the microphone captures but there is no A2DP transport",
                "7 microphone SDUs dropped",
            ]
        );
    }

    #[test]
    fn a_transport_with_an_unknown_state_is_not_reported() {
        let unknown = Health {
            transport: Some(TransportView {
                state: None,
                codec: None,
                age: Duration::from_secs(30),
            }),
            ..healthy()
        };
        assert_eq!(unknown.problems(), []);
        assert!(
            unknown
                .to_string()
                .starts_with("transport=unknown for 30s owns=yes")
        );
    }

    #[test]
    fn problems_are_logged_when_they_change_not_while_they_last() {
        let idle = |secs| {
            Health {
                transport: Some(transport(TransportState::Idle, secs)),
                ..healthy()
            }
            .problems()
        };
        let with_drops = |n| {
            let mut problems = idle(30);
            problems.push(Problem::SdusDropped(n));
            problems
        };
        let mut log = ProblemLog::default();

        assert_eq!(log.update(&[]), Change::Same);
        assert_eq!(log.update(&idle(10)), Change::Changed);
        // The transport stays idle; only its age moved.
        assert_eq!(log.update(&idle(20)), Change::Same);
        assert_eq!(log.update(&with_drops(3)), Change::Changed);
        assert_eq!(log.update(&with_drops(9)), Change::Same);
        assert_eq!(log.update(&idle(40)), Change::Changed);
        assert_eq!(log.update(&[]), Change::Cleared);
        assert_eq!(log.update(&[]), Change::Same);
    }

    #[test]
    fn sdu_counts_since_never_go_negative() {
        let earlier = SduCounts {
            received: 10,
            dropped: 2,
        };
        let now = SduCounts {
            received: 25,
            dropped: 2,
        };
        assert_eq!(
            now.since(earlier),
            SduCounts {
                received: 15,
                dropped: 0
            }
        );
        assert_eq!(earlier.since(now), SduCounts::default());
    }
}

//! Local media players over MPRIS on the D-Bus session bus, behind the
//! `MediaPlayers` seam the media controller, auto-switch and the mic test use.

use {
    dbus::blocking::{Connection, stdintf::org_freedesktop_dbus::Properties},
    std::{fmt, time::Duration},
    thiserror::Error,
    tracing::{error, info},
};

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const DBUS_TIMEOUT: Duration = Duration::from_secs(5);

/// A failed MPRIS request. The cause is part of the message because these are
/// logged with `{}` where they are handled.
#[derive(Debug, Error)]
pub enum MediaPlayerError {
    #[error("could not connect to the D-Bus session bus: {0}")]
    SessionBus(dbus::Error),
    #[error("could not list the D-Bus names: {0}")]
    ListNames(dbus::Error),
    #[error("{command} failed for {service}: {err}")]
    Command {
        service: String,
        command: PlayerCommand,
        err: dbus::Error,
    },
    /// The blocking task running the request was cancelled, which happens
    /// when the runtime shuts down.
    #[error("the media player request was interrupted")]
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerCommand {
    Play,
    Pause,
    Next,
    Previous,
}

impl PlayerCommand {
    fn method(self) -> &'static str {
        match self {
            Self::Play => "Play",
            Self::Pause => "Pause",
            Self::Next => "Next",
            Self::Previous => "Previous",
        }
    }
}

impl fmt::Display for PlayerCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.method())
    }
}

/// One MPRIS player and whether it reported "Playing".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerState {
    pub service: String,
    pub playing: bool,
}

/// The local media players. Every call blocks on D-Bus for up to a few
/// seconds, so async callers run it on the blocking pool.
pub trait MediaPlayers: Send + Sync {
    /// Every player in bus order, kdeconnect proxies of phone players excluded.
    fn players(&self) -> Result<Vec<PlayerState>, MediaPlayerError>;
    fn send(&self, service: &str, command: PlayerCommand) -> Result<(), MediaPlayerError>;
}

/// Services of the players that are playing right now.
pub fn playing(players: &dyn MediaPlayers) -> Result<Vec<String>, MediaPlayerError> {
    Ok(players
        .players()?
        .into_iter()
        .filter(|p| p.playing)
        .map(|p| p.service)
        .collect())
}

/// Pause every playing player and return the ones that paused, so the caller
/// can resume exactly those later.
pub fn pause_playing(players: &dyn MediaPlayers) -> Result<Vec<String>, MediaPlayerError> {
    let mut paused = Vec::new();
    for service in playing(players)? {
        match players.send(&service, PlayerCommand::Pause) {
            Ok(()) => {
                info!("Paused playback for: {}", service);
                paused.push(service);
            },
            Err(e) => error!("{}", e),
        }
    }
    Ok(paused)
}

/// The D-Bus session bus, one connection per request.
pub struct DbusMediaPlayers;

impl DbusMediaPlayers {
    fn connect() -> Result<Connection, MediaPlayerError> {
        Connection::new_session().map_err(MediaPlayerError::SessionBus)
    }
}

impl MediaPlayers for DbusMediaPlayers {
    fn players(&self) -> Result<Vec<PlayerState>, MediaPlayerError> {
        let conn = Self::connect()?;
        let proxy = conn.with_proxy(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            DBUS_TIMEOUT,
        );
        let (names,): (Vec<String>,) = proxy
            .method_call("org.freedesktop.DBus", "ListNames", ())
            .map_err(MediaPlayerError::ListNames)?;
        Ok(names
            .into_iter()
            .filter(|name| is_player(name))
            .map(|service| {
                let proxy = conn.with_proxy(&service, MPRIS_PATH, DBUS_TIMEOUT);
                // A player that does not answer is treated as not playing.
                let playing = proxy
                    .get::<String>(PLAYER_INTERFACE, "PlaybackStatus")
                    .is_ok_and(|status| status == "Playing");
                PlayerState { service, playing }
            })
            .collect())
    }

    fn send(&self, service: &str, command: PlayerCommand) -> Result<(), MediaPlayerError> {
        let conn = Self::connect()?;
        let proxy = conn.with_proxy(service, MPRIS_PATH, DBUS_TIMEOUT);
        proxy
            .method_call::<(), _, &str, &str>(PLAYER_INTERFACE, command.method(), ())
            .map_err(|err| MediaPlayerError::Command {
                service: service.to_string(),
                command,
                err,
            })
    }
}

/// An MPRIS player on this machine. KDE Connect mirrors the phone's players
/// onto the bus; pausing or counting those would act on the phone.
fn is_player(bus_name: &str) -> bool {
    bus_name.starts_with(MPRIS_PREFIX)
        && !bus_name.starts_with("org.mpris.MediaPlayer2.kdeconnect.mpris_")
}

#[cfg(test)]
pub mod fake {
    //! An in-memory set of players for tests.

    use {
        super::{MediaPlayerError, MediaPlayers, PlayerCommand, PlayerState},
        std::sync::Mutex,
    };

    #[derive(Default)]
    pub struct FakePlayers {
        players: Mutex<Vec<PlayerState>>,
        sent: Mutex<Vec<(String, PlayerCommand)>>,
    }

    impl FakePlayers {
        pub fn set(&self, service: &str, playing: bool) {
            let mut players = self.players.lock().unwrap();
            match players.iter_mut().find(|p| p.service == service) {
                Some(p) => p.playing = playing,
                None => players.push(PlayerState {
                    service: service.to_string(),
                    playing,
                }),
            }
        }

        pub fn sent(&self) -> Vec<(String, PlayerCommand)> {
            self.sent.lock().unwrap().clone()
        }

        pub fn clear_sent(&self) {
            self.sent.lock().unwrap().clear();
        }
    }

    impl MediaPlayers for FakePlayers {
        fn players(&self) -> Result<Vec<PlayerState>, MediaPlayerError> {
            Ok(self.players.lock().unwrap().clone())
        }

        fn send(&self, service: &str, command: PlayerCommand) -> Result<(), MediaPlayerError> {
            self.sent
                .lock()
                .unwrap()
                .push((service.to_string(), command));
            match command {
                PlayerCommand::Play => self.set(service, true),
                PlayerCommand::Pause => self.set(service, false),
                PlayerCommand::Next | PlayerCommand::Previous => {},
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{fake::FakePlayers, *};

    #[test]
    fn kdeconnect_players_are_not_local_players() {
        assert!(is_player("org.mpris.MediaPlayer2.spotify"));
        assert!(!is_player("org.mpris.MediaPlayer2.kdeconnect.mpris_000001"));
        assert!(!is_player("org.freedesktop.Notifications"));
    }

    #[test]
    fn pause_playing_pauses_only_playing_players_and_returns_them() {
        let players = FakePlayers::default();
        players.set("a", true);
        players.set("b", false);
        players.set("c", true);

        let paused = pause_playing(&players).unwrap();

        assert_eq!(paused, ["a", "c"]);
        assert_eq!(
            players.sent(),
            [
                ("a".to_string(), PlayerCommand::Pause),
                ("c".to_string(), PlayerCommand::Pause)
            ]
        );
        assert!(playing(&players).unwrap().is_empty());
    }
}

//! Manual connect requests from the UI or the tray, keyed by MAC. An entry lives
//! until the device connects, the request fails, or the device drops during setup.

use std::collections::HashMap;

/// Shown when the Bluetooth connect worked but DeviceConnected never came.
pub const SETUP_TIMEOUT_ERROR: &str = "Connected, but the AirPods did not respond. Try again.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectStatus {
    /// The connect call for this attempt is running.
    Connecting(u64),
    /// The connect call returned; waiting for DeviceConnected, which comes once
    /// the AACP session is set up.
    SettingUp(u64),
    Failed(String),
}

impl ConnectStatus {
    pub fn in_progress(&self) -> bool {
        matches!(self, Self::Connecting(_) | Self::SettingUp(_))
    }
}

#[derive(Debug, Default)]
pub struct ConnectRequests {
    status: HashMap<String, ConnectStatus>,
    /// Numbers each request, so a late result or timeout from an older request
    /// does not touch a newer one.
    last_attempt: u64,
}

impl ConnectRequests {
    pub fn get(&self, mac: &str) -> Option<&ConnectStatus> {
        self.status.get(mac)
    }

    pub fn in_progress(&self, mac: &str) -> bool {
        self.status.get(mac).is_some_and(ConnectStatus::in_progress)
    }

    /// Records a new attempt for `mac` and returns its number.
    pub fn start(&mut self, mac: String) -> u64 {
        self.last_attempt += 1;
        self.status
            .insert(mac, ConnectStatus::Connecting(self.last_attempt));
        self.last_attempt
    }

    /// Records the result of the connect call. Returns true when the attempt is
    /// still the current one and succeeded, so the setup timeout should start.
    /// The entry stays in progress until DeviceConnected arrives, or the panel
    /// would offer the Connect button again in between.
    pub fn finished(&mut self, mac: &str, attempt: u64, result: Result<(), String>) -> bool {
        let Some(status) = self.status.get_mut(mac) else {
            return false;
        };
        if *status != ConnectStatus::Connecting(attempt) {
            return false;
        }
        let setting_up = result.is_ok();
        *status = match result {
            Ok(()) => ConnectStatus::SettingUp(attempt),
            Err(e) => ConnectStatus::Failed(e),
        };
        setting_up
    }

    /// Fails the attempt if it is still waiting for DeviceConnected.
    pub fn setup_timed_out(&mut self, mac: &str, attempt: u64) {
        if let Some(status) = self.status.get_mut(mac)
            && *status == ConnectStatus::SettingUp(attempt)
        {
            *status = ConnectStatus::Failed(SETUP_TIMEOUT_ERROR.to_string());
        }
    }

    pub fn connected(&mut self, mac: &str) {
        self.status.remove(mac);
    }

    /// A device that drops while its session is being set up ends the request;
    /// a failure stays on screen.
    pub fn disconnected(&mut self, mac: &str) {
        if matches!(self.status.get(mac), Some(ConnectStatus::SettingUp(_))) {
            self.status.remove(mac);
        }
    }
}

/// The sidebar's one-line status for a device that is not connected.
pub fn sidebar_status(status: Option<&ConnectStatus>) -> &'static str {
    match status {
        Some(ConnectStatus::Connecting(_) | ConnectStatus::SettingUp(_)) => "Connecting…",
        Some(ConnectStatus::Failed(_)) => "Couldn't connect",
        None => "Not connected",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: &str = "AA:BB:CC:DD:EE:FF";

    #[test]
    fn start_numbers_each_attempt() {
        let mut requests = ConnectRequests::default();

        let first = requests.start(MAC.to_string());
        let second = requests.start(MAC.to_string());

        assert_eq!(second, first + 1);
        assert_eq!(requests.get(MAC), Some(&ConnectStatus::Connecting(second)));
        assert!(requests.in_progress(MAC));
    }

    #[test]
    fn successful_connect_waits_for_setup() {
        let mut requests = ConnectRequests::default();
        let attempt = requests.start(MAC.to_string());

        let start_timeout = requests.finished(MAC, attempt, Ok(()));

        assert!(start_timeout);
        assert_eq!(requests.get(MAC), Some(&ConnectStatus::SettingUp(attempt)));
        assert!(requests.in_progress(MAC));
    }

    #[test]
    fn failed_connect_keeps_the_error() {
        let mut requests = ConnectRequests::default();
        let attempt = requests.start(MAC.to_string());

        let start_timeout = requests.finished(MAC, attempt, Err("refused".to_string()));

        assert!(!start_timeout);
        assert_eq!(
            requests.get(MAC),
            Some(&ConnectStatus::Failed("refused".to_string()))
        );
        assert!(!requests.in_progress(MAC));
    }

    #[test]
    fn result_of_an_older_attempt_is_ignored() {
        let mut requests = ConnectRequests::default();
        let old = requests.start(MAC.to_string());
        let new = requests.start(MAC.to_string());

        let start_timeout = requests.finished(MAC, old, Err("late".to_string()));

        assert!(!start_timeout);
        assert_eq!(requests.get(MAC), Some(&ConnectStatus::Connecting(new)));
    }

    #[test]
    fn setup_timeout_fails_only_the_same_attempt() {
        let mut requests = ConnectRequests::default();
        let old = requests.start(MAC.to_string());
        requests.finished(MAC, old, Ok(()));
        let new = requests.start(MAC.to_string());
        requests.finished(MAC, new, Ok(()));

        requests.setup_timed_out(MAC, old);
        assert_eq!(requests.get(MAC), Some(&ConnectStatus::SettingUp(new)));

        requests.setup_timed_out(MAC, new);
        assert_eq!(
            requests.get(MAC),
            Some(&ConnectStatus::Failed(SETUP_TIMEOUT_ERROR.to_string()))
        );
    }

    #[test]
    fn setup_timeout_after_connect_does_nothing() {
        let mut requests = ConnectRequests::default();
        let attempt = requests.start(MAC.to_string());
        requests.finished(MAC, attempt, Ok(()));
        requests.connected(MAC);

        requests.setup_timed_out(MAC, attempt);

        assert_eq!(requests.get(MAC), None);
    }

    #[test]
    fn disconnect_clears_only_a_request_in_setup() {
        let mut requests = ConnectRequests::default();
        let attempt = requests.start(MAC.to_string());

        requests.disconnected(MAC);
        assert_eq!(requests.get(MAC), Some(&ConnectStatus::Connecting(attempt)));

        requests.finished(MAC, attempt, Ok(()));
        requests.disconnected(MAC);
        assert_eq!(requests.get(MAC), None);

        let attempt = requests.start(MAC.to_string());
        requests.finished(MAC, attempt, Err("refused".to_string()));
        requests.disconnected(MAC);
        assert!(matches!(requests.get(MAC), Some(ConnectStatus::Failed(_))));
    }

    #[test]
    fn sidebar_status_names_each_state() {
        assert_eq!(sidebar_status(None), "Not connected");
        assert_eq!(
            sidebar_status(Some(&ConnectStatus::Connecting(1))),
            "Connecting…"
        );
        assert_eq!(
            sidebar_status(Some(&ConnectStatus::SettingUp(1))),
            "Connecting…"
        );
        assert_eq!(
            sidebar_status(Some(&ConnectStatus::Failed("x".to_string()))),
            "Couldn't connect"
        );
    }
}

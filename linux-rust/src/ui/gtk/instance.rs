//! Keep one LibrePods per session.
//!
//! GApplication already routes a second launch to the running instance, but
//! only once the UI starts; by then main would have started a second Bluetooth
//! backend competing for the same AirPods connection. So main asks first.

use {
    super::window::APP_ID,
    gtk::gio::{self, prelude::*},
};

/// If LibrePods already runs in this session, show its window and return true.
pub fn present_running_instance() -> bool {
    let probe = gio::Application::new(Some(APP_ID), gio::ApplicationFlags::default());
    if let Err(e) = probe.register(gio::Cancellable::NONE) {
        // Without a session bus there is nothing to hand over to; start normally.
        tracing::warn!("Could not check for a running instance: {e}");
        return false;
    }
    if probe.is_remote() {
        probe.activate();
        return true;
    }
    false
}

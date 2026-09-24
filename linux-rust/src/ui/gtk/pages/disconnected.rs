//! Shown for known AirPods that are not connected to this PC.

use {
    crate::ui::gtk::{
        model::{DisconnectedView, Input},
        widgets::{CurrentDevice, Dispatch},
    },
    adw::prelude::*,
};

pub(crate) struct DisconnectedPage {
    status: adw::StatusPage,
    connect: gtk::Button,
    mac: CurrentDevice,
}

impl DisconnectedPage {
    pub(crate) fn new(dispatch: &Dispatch) -> Self {
        let connect = gtk::Button::builder()
            .label("Connect to this PC")
            .halign(gtk::Align::Center)
            .css_classes(["suggested-action", "pill"])
            .build();
        let status = adw::StatusPage::builder()
            .icon_name("audio-headphones-symbolic")
            .title("AirPods not connected")
            .child(&connect)
            .vexpand(true)
            .build();
        let mac = CurrentDevice::default();
        let dispatch = dispatch.clone();
        let clicked_mac = mac.clone();
        connect.connect_clicked(move |_| dispatch.send(Input::Connect(clicked_mac.get())));
        Self {
            status,
            connect,
            mac,
        }
    }

    pub(crate) fn widget(&self) -> &adw::StatusPage {
        &self.status
    }

    pub(crate) fn render(&self, mac: &str, view: &DisconnectedView) {
        self.mac.set(mac);
        self.status.set_description(Some(&view.status));
        self.connect.set_label(view.button_label());
        self.connect.set_sensitive(!view.connecting);
    }
}

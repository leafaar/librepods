//! The main window: a leaflet with the sidebar and the content pane, each
//! under its own header bar, inside a toast overlay.

use {
    crate::ui::gtk::{
        model::{Content, Model},
        pages::{
            airpods::AirPodsPage, disconnected::DisconnectedPage, nothing::NothingPage,
            settings::SettingsPage,
        },
        sidebar::Sidebar,
        widgets::Dispatch,
    },
    adw::prelude::*,
};

/// Application id; also the .desktop file name and the window icon name.
pub(crate) const APP_ID: &str = "me.kavishdevar.librepods";

const PAGE_AIRPODS: &str = "airpods";
const PAGE_DISCONNECTED: &str = "disconnected";
const PAGE_NOTHING: &str = "nothing";
const PAGE_SETTINGS: &str = "settings";
const PAGE_STATUS: &str = "status";

pub(crate) struct Window {
    root: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    title: adw::WindowTitle,
    stack: gtk::Stack,
    /// Placeholder for the pages that are only a message.
    status: adw::StatusPage,
    sidebar: Sidebar,
    airpods: AirPodsPage,
    disconnected: DisconnectedPage,
    nothing: NothingPage,
    settings: SettingsPage,
}

impl Window {
    pub(crate) fn new(app: &adw::Application, dispatch: &Dispatch) -> Self {
        let sidebar = Sidebar::new(dispatch);
        let airpods = AirPodsPage::new(dispatch);
        let disconnected = DisconnectedPage::new(dispatch);
        let nothing = NothingPage::new(dispatch);
        let settings = SettingsPage::new(dispatch);
        let status = adw::StatusPage::builder().vexpand(true).build();

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .build();
        stack.add_named(airpods.widget(), Some(PAGE_AIRPODS));
        stack.add_named(disconnected.widget(), Some(PAGE_DISCONNECTED));
        stack.add_named(nothing.widget(), Some(PAGE_NOTHING));
        stack.add_named(settings.widget(), Some(PAGE_SETTINGS));
        stack.add_named(&status, Some(PAGE_STATUS));

        let leaflet = adw::Leaflet::builder().can_navigate_back(true).build();

        let sidebar_header = adw::HeaderBar::builder()
            .title_widget(&adw::WindowTitle::new("LibrePods", ""))
            .show_end_title_buttons(false)
            .build();
        let sidebar_pane = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .width_request(260)
            .build();
        sidebar_pane.append(&sidebar_header);
        sidebar_pane.append(sidebar.widget());

        let title = adw::WindowTitle::new("", "");
        let back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Back")
            .visible(false)
            .build();
        let content_header = adw::HeaderBar::builder()
            .title_widget(&title)
            .show_start_title_buttons(false)
            .build();
        content_header.pack_start(&back);
        let content_pane = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .build();
        content_pane.append(&content_header);
        content_pane.append(&stack);

        leaflet.append(&sidebar_pane);
        leaflet
            .append(&gtk::Separator::new(gtk::Orientation::Vertical))
            .set_navigatable(false);
        leaflet.append(&content_pane);

        // Folded, each pane fills the window and carries the window buttons,
        // and the content pane gets a way back to the list.
        for (target, property) in [
            (
                sidebar_header.upcast_ref::<gtk::Widget>(),
                "show-end-title-buttons",
            ),
            (content_header.upcast_ref(), "show-start-title-buttons"),
            (back.upcast_ref(), "visible"),
        ] {
            leaflet
                .bind_property("folded", target, property)
                .sync_create()
                .build();
        }
        {
            let leaflet = leaflet.clone();
            back.connect_clicked(move |_| {
                leaflet.navigate(adw::NavigationDirection::Back);
            });
        }
        for list in [sidebar.device_list(), sidebar.settings_list()] {
            let leaflet = leaflet.clone();
            list.connect_row_activated(move |_, _| {
                leaflet.navigate(adw::NavigationDirection::Forward);
            });
        }

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&leaflet));
        let root = adw::ApplicationWindow::builder()
            .application(app)
            .title("LibrePods")
            .icon_name(APP_ID)
            .default_width(900)
            .default_height(660)
            .width_request(360)
            .height_request(400)
            .content(&toasts)
            .build();

        Self {
            root,
            toasts,
            title,
            stack,
            status,
            sidebar,
            airpods,
            disconnected,
            nothing,
            settings,
        }
    }

    pub(crate) fn root(&self) -> &adw::ApplicationWindow {
        &self.root
    }

    pub(crate) fn present(&self) {
        self.root.present();
    }

    pub(crate) fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    pub(crate) fn render(&self, model: &Model) {
        self.sidebar.render(model);
        let (page, title) = match model.content() {
            Content::Empty => {
                self.show_status(
                    "bluetooth-symbolic",
                    "No devices",
                    "Connect your AirPods to this PC once to add them here.",
                );
                (PAGE_STATUS, String::new())
            },
            Content::Settings => {
                self.settings.render(model);
                (PAGE_SETTINGS, "Settings".to_string())
            },
            Content::AirPods(mac) => {
                if let Some(airpods) = model.airpods(&mac) {
                    self.airpods.render(&mac, airpods, model);
                }
                (PAGE_AIRPODS, model.title(&mac).to_string())
            },
            Content::Waiting(mac) => {
                self.show_status(
                    "audio-headphones-symbolic",
                    "Connected",
                    "Waiting for the device to report its state…",
                );
                (PAGE_STATUS, model.title(&mac).to_string())
            },
            Content::Disconnected(mac) => {
                self.disconnected.render(&mac, &model.disconnected(&mac));
                (PAGE_DISCONNECTED, model.title(&mac).to_string())
            },
            Content::Nothing(mac) => {
                if let Some(nothing) = model.nothing(&mac) {
                    self.nothing.render(&mac, nothing);
                }
                (PAGE_NOTHING, model.title(&mac).to_string())
            },
            Content::Unavailable(mac) => {
                self.show_status(
                    "audio-headphones-symbolic",
                    "Not connected",
                    "Connect the device to this PC from the Bluetooth settings.",
                );
                (PAGE_STATUS, model.title(&mac).to_string())
            },
        };
        if self.stack.visible_child_name().as_deref() != Some(page) {
            self.stack.set_visible_child_name(page);
        }
        if self.title.title() != title {
            self.title.set_title(&title);
        }
    }

    fn show_status(&self, icon: &str, title: &str, description: &str) {
        self.status.set_icon_name(Some(icon));
        self.status.set_title(title);
        self.status.set_description(Some(description));
    }
}

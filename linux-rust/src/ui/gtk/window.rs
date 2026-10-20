//! The main window: a split view with the sidebar and the content pane, each
//! under its own header bar, inside a toast overlay. A narrow window shows one
//! pane at a time.

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

/// Narrowest the sidebar gets while both panes show, in sp.
const SIDEBAR_WIDTH: f64 = 260.0;
/// Below this window width, in sp, only one pane shows at a time.
const COLLAPSE_BELOW: f64 = 600.0;

pub(crate) struct Window {
    root: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    /// The content pane; its title is the header bar title.
    content: adw::NavigationPage,
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

        let sidebar_page = adw::NavigationPage::builder()
            .title("LibrePods")
            .child(&toolbar(sidebar.widget()))
            .build();
        let content = adw::NavigationPage::builder()
            .title("")
            .child(&toolbar(&stack))
            .build();
        let split = adw::NavigationSplitView::builder()
            .sidebar(&sidebar_page)
            .content(&content)
            .min_sidebar_width(SIDEBAR_WIDTH)
            .build();
        // Collapsed, the split view shows one pane at a time and gives the
        // content pane a back button; picking an entry shows its content.
        for list in [sidebar.device_list(), sidebar.settings_list()] {
            let split = split.clone();
            list.connect_row_activated(move |_, _| split.set_show_content(true));
        }

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&split));
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
        let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            COLLAPSE_BELOW,
            adw::LengthUnit::Sp,
        ));
        narrow.add_setter(&split, "collapsed", Some(&true.to_value()));
        root.add_breakpoint(narrow);

        Self {
            root,
            toasts,
            content,
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

    #[cfg(test)]
    pub(crate) fn sidebar(&self) -> &Sidebar {
        &self.sidebar
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
        if self.content.title() != title {
            self.content.set_title(&title);
        }
    }

    fn show_status(&self, icon: &str, title: &str, description: &str) {
        self.status.set_icon_name(Some(icon));
        self.status.set_title(title);
        self.status.set_description(Some(description));
    }
}

/// A pane under its own header bar.
fn toolbar(child: &impl IsA<gtk::Widget>) -> adw::ToolbarView {
    let view = adw::ToolbarView::builder().content(child).build();
    view.add_top_bar(&adw::HeaderBar::new());
    view
}

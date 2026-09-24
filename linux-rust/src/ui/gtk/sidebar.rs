//! Device list and the Settings entry.

use {
    crate::ui::gtk::{
        battery::Batteries,
        model::{Input, Model, Selection, SidebarEntry, SidebarStatus},
        widgets::{Dispatch, Guarded, clear_box},
    },
    adw::prelude::*,
    std::{cell::RefCell, rc::Rc},
};

pub(crate) struct Sidebar {
    root: gtk::Box,
    devices: Guarded<gtk::ListBox>,
    settings: Guarded<gtk::ListBox>,
    settings_row: gtk::ListBoxRow,
    /// Rows in list order, with the entry each was last drawn from. Shared
    /// with the selection handler to map a row back to its device.
    rows: Rc<RefCell<Vec<DeviceRow>>>,
}

struct DeviceRow {
    mac: String,
    row: gtk::ListBoxRow,
    name: gtk::Label,
    connected: gtk::Image,
    status: gtk::Box,
    drawn: Option<SidebarEntry>,
}

impl Sidebar {
    pub(crate) fn new(dispatch: &Dispatch) -> Self {
        let rows: Rc<RefCell<Vec<DeviceRow>>> = Rc::default();
        let devices = gtk::ListBox::builder()
            .css_classes(["navigation-sidebar"])
            .build();
        let devices = {
            let dispatch = dispatch.clone();
            let rows = Rc::clone(&rows);
            Guarded::new(devices, move |list| {
                list.connect_row_selected(move |_, row| {
                    let Some(row) = row else { return };
                    let mac = usize::try_from(row.index())
                        .ok()
                        .and_then(|i| rows.borrow().get(i).map(|r| r.mac.clone()));
                    if let Some(mac) = mac {
                        dispatch.send(Input::Select(Selection::Device(mac)));
                    }
                })
            })
        };

        let settings_row = gtk::ListBoxRow::builder()
            .child(&row_label("Settings", "emblem-system-symbolic"))
            .build();
        let settings = gtk::ListBox::builder()
            .css_classes(["navigation-sidebar"])
            .build();
        settings.append(&settings_row);
        let settings = {
            let dispatch = dispatch.clone();
            Guarded::new(settings, move |list| {
                list.connect_row_selected(move |_, row| {
                    if row.is_some() {
                        dispatch.send(Input::Select(Selection::Settings));
                    }
                })
            })
        };

        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(devices.widget())
            .build();
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        root.append(&scrolled);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        root.append(settings.widget());
        Self {
            root,
            devices,
            settings,
            settings_row,
            rows,
        }
    }

    pub(crate) fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// The device list, for navigation on activation.
    pub(crate) fn device_list(&self) -> &gtk::ListBox {
        self.devices.widget()
    }

    pub(crate) fn settings_list(&self) -> &gtk::ListBox {
        self.settings.widget()
    }

    pub(crate) fn render(&self, model: &Model) {
        let entries = model.sidebar();
        let same_devices = {
            let rows = self.rows.borrow();
            rows.len() == entries.len() && rows.iter().zip(&entries).all(|(r, e)| r.mac == e.mac)
        };
        if !same_devices {
            self.rebuild(&entries);
        }
        for (row, entry) in self.rows.borrow_mut().iter_mut().zip(entries) {
            row.draw(entry);
        }
        self.render_selection(model.selection());
    }

    fn rebuild(&self, entries: &[SidebarEntry]) {
        let mut rows = self.rows.borrow_mut();
        self.devices.set(|list| {
            for row in rows.drain(..) {
                list.remove(&row.row);
            }
            for entry in entries {
                let row = DeviceRow::new(entry.mac.clone());
                list.append(&row.row);
                rows.push(row);
            }
        });
    }

    fn render_selection(&self, selection: &Selection) {
        let device_row = match selection {
            Selection::Device(mac) => self
                .rows
                .borrow()
                .iter()
                .find(|r| r.mac == *mac)
                .map(|r| r.row.clone()),
            Selection::None | Selection::Settings => None,
        };
        if self.devices.widget().selected_row() != device_row {
            self.devices
                .set(|list| list.select_row(device_row.as_ref()));
        }
        let settings_selected = *selection == Selection::Settings;
        if self.settings.widget().selected_row().is_some() != settings_selected {
            self.settings.set(|list| {
                list.select_row(settings_selected.then_some(&self.settings_row));
            });
        }
    }
}

impl DeviceRow {
    fn new(mac: String) -> Self {
        let name = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .hexpand(true)
            .build();
        let connected = gtk::Image::builder()
            .icon_name("bluetooth-active-symbolic")
            .tooltip_text("Connected")
            .build();
        let title = gtk::Box::builder().spacing(6).build();
        title.append(&name);
        title.append(&connected);
        let status = gtk::Box::builder().spacing(8).build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        content.append(&title);
        content.append(&status);
        let row = gtk::ListBoxRow::builder().child(&content).build();
        Self {
            mac,
            row,
            name,
            connected,
            status,
            drawn: None,
        }
    }

    fn draw(&mut self, entry: SidebarEntry) {
        if self.drawn.as_ref() == Some(&entry) {
            return;
        }
        self.name.set_label(&entry.name);
        self.connected.set_visible(entry.connected);
        clear_box(&self.status);
        match &entry.status {
            SidebarStatus::Text(text) => self.status.append(&dim_caption(text)),
            SidebarStatus::Batteries(batteries) => {
                let headphone = matches!(batteries, Batteries::Headphone(_));
                for (label, level) in batteries.labeled() {
                    let part = gtk::Box::builder().spacing(2).build();
                    part.append(
                        &gtk::Image::builder()
                            .icon_name(level.icon_name())
                            .pixel_size(16)
                            .build(),
                    );
                    let text = match label.chars().next() {
                        // "L 80%": the first letter of Left, Right or Case.
                        Some(initial) if !headphone => format!("{initial} {}", level.text()),
                        _ => level.text(),
                    };
                    part.append(
                        &gtk::Label::builder()
                            .label(text)
                            .css_classes(["caption"])
                            .build(),
                    );
                    if level.stale {
                        part.add_css_class("dim-label");
                        part.set_tooltip_text(Some("Last known case level"));
                    }
                    self.status.append(&part);
                }
            },
        }
        self.drawn = Some(entry);
    }
}

fn dim_caption(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .build()
}

fn row_label(text: &str, icon: &str) -> gtk::Box {
    let content = gtk::Box::builder()
        .spacing(12)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    content.append(&gtk::Image::from_icon_name(icon));
    content.append(&gtk::Label::new(Some(text)));
    content
}

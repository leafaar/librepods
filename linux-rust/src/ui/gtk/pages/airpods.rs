//! The page of connected AirPods: battery, name, noise control and device
//! information, plus the sections other parts of the app add.
//!
//! The page is a list of `Section`s, each an adw::PreferencesGroup built once
//! with its handlers and redrawn from the model by `render`. To add a block,
//! write a type that implements `Section` (in its own file under pages/ when
//! it is more than a few rows) and put it into the list in `AirPodsPage::new`
//! at the place marked for it.

use {
    crate::{
        devices::enums::AirPodsNoiseControlMode,
        ui::gtk::{
            battery::{Batteries, Level},
            model::{AirPods, Input, Model, NameHint},
            pages::{
                airpods_settings::ControlGroup, equalizer::EqualizerSection,
                microphone::MicrophoneSection,
            },
            widgets::{
                CurrentDevice, Dispatch, Guarded, clear_box, combo_row, switch_row, value_row,
            },
        },
    },
    adw::prelude::*,
    std::{cell::RefCell, rc::Rc},
};

/// A block of the AirPods page.
pub(crate) trait Section {
    fn group(&self) -> &adw::PreferencesGroup;

    /// Set the widgets from the model. Runs after every update while the
    /// page shows the AirPods at `mac`; set input widgets through `Guarded`.
    fn render(&self, mac: &str, airpods: &AirPods, model: &Model);
}

/// What a section needs to build its handlers.
pub(crate) struct SectionContext<'a> {
    pub(crate) dispatch: &'a Dispatch,
    /// The AirPods the page shows when a handler fires.
    pub(crate) mac: &'a CurrentDevice,
}

pub(crate) struct AirPodsPage {
    page: adw::PreferencesPage,
    mac: CurrentDevice,
    sections: Vec<Box<dyn Section>>,
}

impl AirPodsPage {
    pub(crate) fn new(dispatch: &Dispatch) -> Self {
        let mac = CurrentDevice::default();
        let cx = SectionContext {
            dispatch,
            mac: &mac,
        };
        let sections: Vec<Box<dyn Section>> = vec![
            Box::new(BatterySection::new()),
            Box::new(NameSection::new(&cx)),
            Box::new(NoiseControlSection::new(&cx)),
            Box::new(EqualizerSection::new(&cx)),
            Box::new(MicrophoneSection::new(&cx)),
            // AirPods settings sections (pages/airpods_settings.rs).
            Box::new(ControlGroup::press_and_hold(&cx)),
            Box::new(ControlGroup::calls(&cx)),
            Box::new(ControlGroup::microphone(&cx)),
            Box::new(ControlGroup::sleep(&cx)),
            Box::new(ControlGroup::accessibility(&cx)),
            Box::new(ControlGroup::adaptive_audio(&cx)),
            Box::new(InfoSection::new(&cx)),
        ];
        let page = adw::PreferencesPage::new();
        for section in &sections {
            page.add(section.group());
        }
        Self {
            page,
            mac,
            sections,
        }
    }

    pub(crate) fn widget(&self) -> &adw::PreferencesPage {
        &self.page
    }

    pub(crate) fn render(&self, mac: &str, airpods: &AirPods, model: &Model) {
        self.mac.set(mac);
        for section in &self.sections {
            section.render(mac, airpods, model);
        }
    }
}

/// Battery levels as large icons with the percentage under each.
struct BatterySection {
    group: adw::PreferencesGroup,
    levels: gtk::Box,
    /// The labels the levels box was last built for, to rebuild it only when
    /// the device kind changes.
    shown: RefCell<Vec<&'static str>>,
}

impl BatterySection {
    fn new() -> Self {
        let levels = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .homogeneous(true)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let group = adw::PreferencesGroup::new();
        group.add(&levels);
        Self {
            group,
            levels,
            shown: RefCell::new(Vec::new()),
        }
    }
}

impl Section for BatterySection {
    fn group(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    fn render(&self, mac: &str, _airpods: &AirPods, model: &Model) {
        let Some(batteries) = model.batteries(mac) else {
            return;
        };
        let labeled = batteries.labeled();
        let labels: Vec<&'static str> = labeled.iter().map(|(label, _)| *label).collect();
        if *self.shown.borrow() != labels {
            clear_box(&self.levels);
            for label in &labels {
                self.levels.append(&battery_card(label));
            }
            *self.shown.borrow_mut() = labels;
        }
        let mut card = self.levels.first_child();
        for (_, level) in labeled {
            let Some(widget) = card else { break };
            update_battery_card(&widget, level);
            card = widget.next_sibling();
        }
        self.group.set_visible(!matches!(
            batteries,
            Batteries::Buds { left, right, case }
                if [left, right, case].iter().all(|l| l.percent.is_none())
        ));
    }
}

/// A vertical box of icon, percentage and caption.
fn battery_card(caption: &str) -> gtk::Box {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();
    card.append(&gtk::Image::builder().pixel_size(32).build());
    card.append(&gtk::Label::builder().css_classes(["title-3"]).build());
    card.append(
        &gtk::Label::builder()
            .label(caption)
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    card
}

fn update_battery_card(card: &gtk::Widget, level: Level) {
    let (Some(icon), Some(percent)) = (
        card.first_child().and_downcast::<gtk::Image>(),
        card.first_child()
            .and_then(|w| w.next_sibling())
            .and_downcast::<gtk::Label>(),
    ) else {
        return;
    };
    icon.set_icon_name(Some(&level.icon_name()));
    percent.set_label(&level.text());
    // A remembered case level is drawn dimmed.
    for widget in [icon.upcast_ref::<gtk::Widget>(), percent.upcast_ref()] {
        if level.stale {
            widget.add_css_class("dim-label");
        } else {
            widget.remove_css_class("dim-label");
        }
    }
    card.set_tooltip_text(level.stale.then_some("Last known level"));
}

/// Name field, renamed on Enter.
struct NameSection {
    group: adw::PreferencesGroup,
    row: adw::ActionRow,
    entry: Guarded<gtk::Entry>,
}

impl NameSection {
    fn new(cx: &SectionContext<'_>) -> Self {
        let entry = gtk::Entry::builder()
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();
        let entry = {
            let dispatch = cx.dispatch.clone();
            let mac = cx.mac.clone();
            Guarded::new(entry, move |e| {
                e.connect_changed(move |e| {
                    dispatch.send(Input::NameEdited(mac.get(), e.text().to_string()));
                })
            })
        };
        {
            let dispatch = cx.dispatch.clone();
            let mac = cx.mac.clone();
            entry.widget().connect_activate(move |e| {
                dispatch.send(Input::Rename(mac.get(), e.text().to_string()));
            });
        }
        let row = adw::ActionRow::builder().title("Name").build();
        row.add_suffix(entry.widget());
        let group = adw::PreferencesGroup::new();
        group.add(&row);
        Self { group, row, entry }
    }
}

impl Section for NameSection {
    fn group(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    fn render(&self, mac: &str, airpods: &AirPods, model: &Model) {
        let hint = model.name_hint(mac);
        // While the user edits, the field keeps the draft.
        if hint.is_none() && self.entry.widget().text() != airpods.name {
            self.entry.set(|e| e.set_text(&airpods.name));
        }
        self.row.set_subtitle(hint.map_or("", NameHint::text));
        if matches!(hint, Some(NameHint::Invalid(_))) {
            self.entry.widget().add_css_class("error");
        } else {
            self.entry.widget().remove_css_class("error");
        }
    }
}

/// Listening mode and the switches that shape it.
struct NoiseControlSection {
    group: adw::PreferencesGroup,
    listening_mode: Guarded<adw::ComboRow>,
    /// Mode bytes in the order the combo row lists them.
    modes: Rc<RefCell<Vec<u8>>>,
    conversation_row: adw::ActionRow,
    conversation_awareness: Guarded<gtk::Switch>,
    personalized_volume: Guarded<gtk::Switch>,
    allow_off: Guarded<gtk::Switch>,
}

impl NoiseControlSection {
    fn new(cx: &SectionContext<'_>) -> Self {
        let modes = Rc::new(RefCell::new(Vec::<u8>::new()));
        let listening_mode = {
            let dispatch = cx.dispatch.clone();
            let mac = cx.mac.clone();
            let modes = Rc::clone(&modes);
            combo_row("Listening Mode", None, &[], move |index| {
                if let Some(byte) = modes.borrow().get(index as usize) {
                    dispatch.send(Input::SetListeningMode(
                        mac.get(),
                        AirPodsNoiseControlMode::from_byte(byte),
                    ));
                }
            })
        };
        let toggle = |input: fn(String, bool) -> Input| {
            let dispatch = cx.dispatch.clone();
            let mac = cx.mac.clone();
            move |on| dispatch.send(input(mac.get(), on))
        };
        let (conversation_row, conversation_awareness) = switch_row(
            "Conversation Awareness",
            Some("Lowers the volume of your audio when it detects that you are speaking."),
            toggle(Input::SetConversationAwareness),
        );
        let (volume_row, personalized_volume) = switch_row(
            "Personalized Volume",
            Some("Adjusts the volume in response to your environment."),
            toggle(Input::SetPersonalizedVolume),
        );
        let (off_row, allow_off) = switch_row(
            "Off Listening Mode",
            Some(
                "When this is on, AirPods listening modes will include an Off option. Loud \
                 sound levels are not reduced when listening mode is set to Off.",
            ),
            toggle(Input::SetAllowOff),
        );
        let group = adw::PreferencesGroup::builder()
            .title("Noise Control")
            .build();
        group.add(listening_mode.widget());
        group.add(&conversation_row);
        group.add(&volume_row);
        group.add(&off_row);
        Self {
            group,
            listening_mode,
            modes,
            conversation_row,
            conversation_awareness,
            personalized_volume,
            allow_off,
        }
    }
}

impl Section for NoiseControlSection {
    fn group(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    fn render(&self, mac: &str, airpods: &AirPods, model: &Model) {
        let available = airpods.listening_modes();
        let bytes: Vec<u8> = available
            .iter()
            .map(AirPodsNoiseControlMode::to_byte)
            .collect();
        if *self.modes.borrow() != bytes {
            let labels: Vec<String> = available.iter().map(ToString::to_string).collect();
            let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
            self.listening_mode
                .set(|row| row.set_model(Some(&gtk::StringList::new(&labels))));
            *self.modes.borrow_mut() = bytes;
        }
        let current = airpods.listening_mode.to_byte();
        let index = self.modes.borrow().iter().position(|b| *b == current);
        self.listening_mode.select(index);
        self.conversation_awareness
            .set_active(airpods.conversation_awareness);
        // Locked while a hi-res capture turns conversation awareness off.
        self.conversation_row
            .set_sensitive(!model.conversation_awareness_locked(mac));
        self.personalized_volume
            .set_active(airpods.personalized_volume);
        self.allow_off.set_active(airpods.allow_off);
    }
}

/// Model, serial numbers and firmware from devices.json.
struct InfoSection {
    group: adw::PreferencesGroup,
    model_number: gtk::Label,
    manufacturer: gtk::Label,
    serial: gtk::Label,
    left_serial: gtk::Label,
    right_serial: gtk::Label,
    firmware: gtk::Label,
    version2: gtk::Label,
    version3: gtk::Label,
}

impl InfoSection {
    fn new(cx: &SectionContext<'_>) -> Self {
        let group = adw::PreferencesGroup::builder()
            .title("Device Information")
            .build();
        let add = |title: &str, copy: bool| {
            let (row, label) = value_row(title);
            if copy {
                row.add_suffix(&copy_button(&label, cx.dispatch));
            }
            group.add(&row);
            label
        };
        let model_number = add("Model Number", false);
        let manufacturer = add("Manufacturer", false);
        let serial = add("Serial Number", true);
        let left_serial = add("Left Serial Number", true);
        let right_serial = add("Right Serial Number", true);
        let firmware = add("Firmware", false);
        let version2 = add("Version 2", false);
        let version3 = add("Version 3", false);
        Self {
            group,
            model_number,
            manufacturer,
            serial,
            left_serial,
            right_serial,
            firmware,
            version2,
            version3,
        }
    }
}

impl Section for InfoSection {
    fn group(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    fn render(&self, mac: &str, _airpods: &AirPods, model: &Model) {
        let Some(info) = model.information(mac) else {
            self.group.set_visible(false);
            return;
        };
        self.group.set_visible(true);
        for (label, value) in [
            (&self.model_number, &info.model_number),
            (&self.manufacturer, &info.manufacturer),
            (&self.serial, &info.serial_number),
            (&self.left_serial, &info.left_serial_number),
            (&self.right_serial, &info.right_serial_number),
            (&self.firmware, &info.version1),
            (&self.version2, &info.version2),
            (&self.version3, &info.version3),
        ] {
            if label.label() != *value {
                label.set_label(value);
            }
        }
    }
}

/// A flat button that copies the text of `label` to the clipboard.
fn copy_button(label: &gtk::Label, dispatch: &Dispatch) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text("Copy")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    let label = label.clone();
    let dispatch = dispatch.clone();
    button.connect_clicked(move |button| {
        button.clipboard().set_text(&label.label());
        dispatch.send(Input::Toast("Copied to clipboard".to_string()));
    });
    button
}

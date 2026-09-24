//! Small widget helpers shared by the pages.
//!
//! Every input widget whose handler sends something (a command to the AirPods,
//! a settings write) is wrapped in `Guarded` when it is built. The view sets
//! its value only through `Guarded::set`, which blocks that handler while the
//! value changes, so redrawing from the model never sends a command back.

use {
    crate::ui::gtk::model::Input,
    adw::prelude::*,
    gtk::glib::{self, SignalHandlerId},
    std::{cell::RefCell, rc::Rc},
    tokio::sync::mpsc::UnboundedSender,
};

/// Queues an `Input` for the app loop. Cheap to clone into signal handlers.
/// Handlers never touch the model themselves: the loop applies inputs one at
/// a time, so an update never runs inside another.
#[derive(Clone)]
pub(crate) struct Dispatch(UnboundedSender<Input>);

impl Dispatch {
    pub(crate) fn new(tx: UnboundedSender<Input>) -> Self {
        Self(tx)
    }

    pub(crate) fn send(&self, input: Input) {
        // The loop only stops when the application does.
        let _ = self.0.send(input);
    }
}

/// A widget with the handler that reacts to user changes.
pub(crate) struct Guarded<W: IsA<glib::Object>> {
    widget: W,
    handler: SignalHandlerId,
}

impl<W: IsA<glib::Object>> Guarded<W> {
    /// Connect the handler with `connect` and keep its id for `set`.
    pub(crate) fn new(widget: W, connect: impl FnOnce(&W) -> SignalHandlerId) -> Self {
        let handler = connect(&widget);
        Self { widget, handler }
    }

    pub(crate) fn widget(&self) -> &W {
        &self.widget
    }

    /// Change the widget from the model without running the handler.
    pub(crate) fn set(&self, change: impl FnOnce(&W)) {
        self.widget.block_signal(&self.handler);
        change(&self.widget);
        self.widget.unblock_signal(&self.handler);
    }
}

impl Guarded<gtk::Switch> {
    pub(crate) fn set_active(&self, active: bool) {
        if self.widget.is_active() != active {
            self.set(|w| w.set_active(active));
        }
    }
}

impl Guarded<adw::ComboRow> {
    /// Select the item at `index`, or nothing for None.
    pub(crate) fn select(&self, index: Option<usize>) {
        let index = index
            .and_then(|i| u32::try_from(i).ok())
            .unwrap_or(gtk::INVALID_LIST_POSITION);
        if self.widget.selected() != index {
            self.set(|w| w.set_selected(index));
        }
    }
}

/// The address of the device a page shows. Handlers built once read it when
/// they fire, so one page serves whichever device is selected.
#[derive(Clone, Default)]
pub(crate) struct CurrentDevice(Rc<RefCell<String>>);

impl CurrentDevice {
    pub(crate) fn get(&self) -> String {
        self.0.borrow().clone()
    }

    pub(crate) fn set(&self, mac: &str) {
        if *self.0.borrow() != mac {
            *self.0.borrow_mut() = mac.to_string();
        }
    }
}

/// A row with a title, an optional subtitle and a switch that reports each
/// user toggle to `on_toggle`.
pub(crate) fn switch_row(
    title: &str,
    subtitle: Option<&str>,
    on_toggle: impl Fn(bool) + 'static,
) -> (adw::ActionRow, Guarded<gtk::Switch>) {
    let row = adw::ActionRow::builder().title(title).build();
    if let Some(subtitle) = subtitle {
        row.set_subtitle(subtitle);
    }
    let switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
    row.add_suffix(&switch);
    row.set_activatable_widget(Some(&switch));
    let switch = Guarded::new(switch, |s| {
        s.connect_active_notify(move |s| on_toggle(s.is_active()))
    });
    (row, switch)
}

/// A combo row over fixed labels that reports the index the user picks.
pub(crate) fn combo_row(
    title: &str,
    subtitle: Option<&str>,
    labels: &[&str],
    on_select: impl Fn(u32) + 'static,
) -> Guarded<adw::ComboRow> {
    let row = adw::ComboRow::builder().title(title).build();
    if let Some(subtitle) = subtitle {
        row.set_subtitle(subtitle);
    }
    row.set_model(Some(&gtk::StringList::new(labels)));
    Guarded::new(row, |r| {
        r.connect_selected_notify(move |r| {
            if r.selected() != gtk::INVALID_LIST_POSITION {
                on_select(r.selected());
            }
        })
    })
}

/// A row showing a value on the right, dimmed and selectable.
pub(crate) fn value_row(title: &str) -> (adw::ActionRow, gtk::Label) {
    let row = adw::ActionRow::builder().title(title).build();
    let label = gtk::Label::builder()
        .selectable(true)
        .valign(gtk::Align::Center)
        .css_classes(["dim-label"])
        .build();
    row.add_suffix(&label);
    (row, label)
}

/// Remove every child of a box.
pub(crate) fn clear_box(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

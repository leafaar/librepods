//! Native GNOME UI with GTK 4 and libadwaita. Targets GTK 4.6 and
//! libadwaita 1.1, so only widgets from those versions are used.
//!
//! # Layout
//!
//! - `model`: all state and decisions, no GTK. `Model::update(Input)` returns
//!   `Effect`s. Unit tested.
//! - `battery`: toolkit-free display decisions the model uses.
//! - `controls`: the AirPods settings driven by control commands: what
//!   shows, what a change sends, and holding slider values until they rest.
//! - `app`: startup, the input loop and the effects (backend commands, file
//!   I/O, timers).
//! - `window`, `sidebar`, `pages/*`: widgets. Built once, then `render(&Model)`
//!   sets them after every update.
//! - `widgets`: `Dispatch`, `Guarded` and row helpers.
//! - `smoke`: an ignored test that renders the real widgets and checks that
//!   rendering sends no input; it needs a display.
//!
//! # Rules for widget code
//!
//! 1. A signal handler only sends an `Input` through `Dispatch`, apart from
//!    purely visual actions (leaflet navigation, copying a label). It never
//!    reads or changes the model and never talks to a device. The loop in
//!    `app` applies inputs one at a time, draws, then runs the effects.
//! 2. Every widget whose handler sends an input is wrapped in `Guarded`, and
//!    `render` changes it only through `Guarded::set` (or `set_active`,
//!    `select`), which blocks the handler. Redrawing from the model must never
//!    send a command to the AirPods.
//! 3. Nothing blocks the GTK thread: device work runs on the backend runtime,
//!    blocking file work in `gio::spawn_blocking` or a worker, and the result
//!    comes back as an `Input`.
//! 4. Decisions (what to show, what to send, validation) go in the model with
//!    a test, not in a handler or in `render`.

mod app;
mod battery;
mod controls;
mod model;
mod pages;
mod sidebar;
#[cfg(test)]
mod smoke;
mod widgets;
mod window;

pub use app::{Options, run};

//! Native GNOME UI with GTK 4 and libadwaita. Targets GTK 4.6 and
//! libadwaita 1.1, so only widgets from those versions are used.
//!
//! - `model`: all state and decisions, no GTK. `Model::update(Input)` returns
//!   `Effect`s. Unit tested.
//! - `battery`, `theme`: toolkit-free display decisions the model uses.

mod battery;
mod model;
mod theme;

//! The equalizer section of the AirPods page: the custom EQ switch, a slider
//! per band and Reset. Hidden for models that do not offer it.

use {
    crate::{
        bluetooth::eq::BAND_MAX,
        ui::gtk::{
            model::{AirPods, EqBand, EqInput, Input, Model},
            pages::airpods::{Section, SectionContext},
            widgets::{Guarded, switch_row},
        },
    },
    adw::prelude::*,
};

pub(crate) struct EqualizerSection {
    group: adw::PreferencesGroup,
    enabled: Guarded<gtk::Switch>,
    reset: gtk::Button,
    /// One row and slider per band, in `EqBand::ALL` order.
    bands: Vec<(adw::ActionRow, Guarded<gtk::Scale>)>,
}

impl EqualizerSection {
    pub(crate) fn new(cx: &SectionContext<'_>) -> Self {
        let (enabled_row, enabled) = {
            let dispatch = cx.dispatch.clone();
            let mac = cx.mac.clone();
            switch_row(
                "Custom EQ",
                Some(
                    "Use your own bass, mids and treble instead of the recommended tuning. \
                     Needs the latest AirPods beta firmware.",
                ),
                move |on| dispatch.send(Input::Equalizer(EqInput::SetEnabled(mac.get(), on))),
            )
        };
        let reset = gtk::Button::builder()
            .label("Reset")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let dispatch = cx.dispatch.clone();
            let mac = cx.mac.clone();
            reset.connect_clicked(move |_| {
                dispatch.send(Input::Equalizer(EqInput::Reset(mac.get())));
            });
        }
        let group = adw::PreferencesGroup::builder()
            .title("Equalizer")
            .header_suffix(&reset)
            .build();
        group.add(&enabled_row);
        let bands = EqBand::ALL
            .into_iter()
            .map(|band| {
                let (row, scale) = band_row(cx, band);
                group.add(&row);
                (row, scale)
            })
            .collect();
        Self {
            group,
            enabled,
            reset,
            bands,
        }
    }
}

/// A row with a 0 to BAND_MAX slider that reports where the user moves it.
fn band_row(cx: &SectionContext<'_>, band: EqBand) -> (adw::ActionRow, Guarded<gtk::Scale>) {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, f64::from(BAND_MAX), 1.0);
    scale.set_digits(0);
    scale.set_round_digits(0);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.set_hexpand(true);
    scale.set_valign(gtk::Align::Center);
    scale.set_width_request(200);
    let scale = {
        let dispatch = cx.dispatch.clone();
        let mac = cx.mac.clone();
        Guarded::new(scale, move |s| {
            s.connect_value_changed(move |s| {
                let value = s.value().round().clamp(0.0, f64::from(BAND_MAX)) as u8;
                dispatch.send(Input::Equalizer(EqInput::SetBand(mac.get(), band, value)));
            })
        })
    };
    let row = adw::ActionRow::builder().title(band.label()).build();
    row.add_suffix(scale.widget());
    (row, scale)
}

impl Section for EqualizerSection {
    fn group(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    fn render(&self, mac: &str, _airpods: &AirPods, model: &Model) {
        let supported = model.equalizer_supported(mac);
        self.group.set_visible(supported);
        if !supported {
            return;
        }
        let eq = model.custom_eq(mac);
        self.enabled.set_active(eq.enabled);
        self.reset.set_visible(eq.enabled);
        self.reset.set_sensitive(model.eq_can_reset(mac));
        for (band, (row, scale)) in EqBand::ALL.into_iter().zip(&self.bands) {
            row.set_visible(eq.enabled);
            let value = f64::from(band.get(eq));
            if (scale.widget().value() - value).abs() > f64::EPSILON {
                scale.set(|s| s.set_value(value));
            }
        }
    }
}

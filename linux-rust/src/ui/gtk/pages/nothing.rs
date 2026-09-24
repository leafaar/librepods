//! Page for a connected Nothing device: its noise control mode.

use {
    crate::ui::gtk::{
        model::{Input, NOTHING_ANC_MODES, Nothing},
        widgets::{CurrentDevice, Dispatch, Guarded, combo_row},
    },
    adw::prelude::*,
};

pub(crate) struct NothingPage {
    page: adw::PreferencesPage,
    anc: Guarded<adw::ComboRow>,
    mac: CurrentDevice,
}

impl NothingPage {
    pub(crate) fn new(dispatch: &Dispatch) -> Self {
        let mac = CurrentDevice::default();
        let labels: Vec<String> = NOTHING_ANC_MODES.iter().map(ToString::to_string).collect();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let anc = {
            let dispatch = dispatch.clone();
            let mac = mac.clone();
            combo_row("Noise Control", None, &labels, move |index| {
                if let Some(mode) = NOTHING_ANC_MODES.get(index as usize) {
                    dispatch.send(Input::SetNothingAnc(mac.get(), mode.clone()));
                }
            })
        };
        let group = adw::PreferencesGroup::new();
        group.add(anc.widget());
        let page = adw::PreferencesPage::new();
        page.add(&group);
        Self { page, anc, mac }
    }

    pub(crate) fn widget(&self) -> &adw::PreferencesPage {
        &self.page
    }

    pub(crate) fn render(&self, mac: &str, nothing: &Nothing) {
        self.mac.set(mac);
        let byte = nothing.anc_mode.to_byte();
        self.anc
            .select(NOTHING_ANC_MODES.iter().position(|m| m.to_byte() == byte));
    }
}

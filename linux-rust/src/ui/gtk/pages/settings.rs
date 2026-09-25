//! App settings: appearance, audio, hi-res microphone and controls.

use {
    crate::{
        ui::gtk::{
            model::{Input, Model, SettingChange},
            widgets::{Dispatch, Guarded, combo_row, switch_row},
        },
        utils::{PreferredCodec, ThemePreference},
    },
    adw::prelude::*,
};

pub(crate) struct SettingsPage {
    page: adw::PreferencesPage,
    theme: Guarded<adw::ComboRow>,
    tray_text_mode: Guarded<gtk::Switch>,
    codec: Guarded<adw::ComboRow>,
    auto_switch: Guarded<gtk::Switch>,
    a2dp_reset: Guarded<gtk::Switch>,
    hires_mic_agc: Guarded<gtk::Switch>,
    hires_mic_pause_convo: Guarded<gtk::Switch>,
    stem_control: Guarded<gtk::Switch>,
}

impl SettingsPage {
    pub(crate) fn new(dispatch: &Dispatch) -> Self {
        let switch = |title, subtitle, change: fn(bool) -> SettingChange| {
            let dispatch = dispatch.clone();
            switch_row(title, Some(subtitle), move |on| {
                dispatch.send(Input::Setting(change(on)));
            })
        };

        let theme = choice_row(
            dispatch,
            "Style",
            None,
            &ThemePreference::ALL,
            ToString::to_string,
            SettingChange::Theme,
        );
        let (tray_row, tray_text_mode) = switch(
            "Use text in tray",
            "Use text for battery status in tray instead of a progress bar.",
            SettingChange::TrayTextMode,
        );
        let appearance = group(
            "Appearance",
            &[theme.widget().upcast_ref(), tray_row.upcast_ref()],
        );

        let codec = choice_row(
            dispatch,
            "Preferred audio codec",
            Some(
                "Codec to activate for playback. The others are used as fallbacks if the chosen \
                 one is unavailable.",
            ),
            &PreferredCodec::ALL,
            ToString::to_string,
            SettingChange::PreferredCodec,
        );
        let (auto_switch_row, auto_switch) = switch(
            "Switch to this PC on playback",
            "When media starts playing here and the AirPods are on another device, connect them \
             to this PC. They leave the other device, even mid-call.",
            SettingChange::AutoSwitchOnPlayback,
        );
        let (a2dp_row, a2dp_reset) = switch(
            "Reset A2DP transport",
            "Briefly suspends and resumes A2DP after the hi-res mic starts or stops. Only turn \
             on if playback stays on one AirPod after capture ends: some setups lose the audio \
             transport after the reset, which cuts calls.",
            SettingChange::A2dpReset,
        );
        let audio = group(
            "Audio",
            &[
                codec.widget().upcast_ref(),
                auto_switch_row.upcast_ref(),
                a2dp_row.upcast_ref(),
            ],
        );

        let (agc_row, hires_mic_agc) = switch(
            "Hi-res mic auto gain",
            "Automatically normalizes the hi-res microphone level. For most uses this should \
             remain on. Disable for a raw, unprocessed capture.",
            SettingChange::HiResMicAgc,
        );
        let (pause_row, hires_mic_pause_convo) = switch(
            "Pause conversation awareness during capture",
            "Turns off conversation awareness while the hi-res microphone is capturing, then \
             restores it afterwards.",
            SettingChange::HiResMicPauseConvo,
        );
        let microphone = group(
            "Hi-Res Microphone",
            &[agc_row.upcast_ref(), pause_row.upcast_ref()],
        );

        let (stem_row, stem_control) = switch(
            "Stem press track control",
            "Double press = next track, triple press = previous track. Disable if your \
             environment handles AirPods AVRCP commands natively.",
            SettingChange::StemControl,
        );
        let controls = group("Controls", &[stem_row.upcast_ref()]);

        let page = adw::PreferencesPage::new();
        for group in [appearance, audio, microphone, controls] {
            page.add(&group);
        }
        Self {
            page,
            theme,
            tray_text_mode,
            codec,
            auto_switch,
            a2dp_reset,
            hires_mic_agc,
            hires_mic_pause_convo,
            stem_control,
        }
    }

    pub(crate) fn widget(&self) -> &adw::PreferencesPage {
        &self.page
    }

    pub(crate) fn render(&self, model: &Model) {
        let settings = model.settings();
        self.theme.select(
            ThemePreference::ALL
                .iter()
                .position(|t| *t == model.theme()),
        );
        self.codec.select(
            PreferredCodec::ALL
                .iter()
                .position(|c| *c == settings.preferred_codec),
        );
        self.tray_text_mode.set_active(settings.tray_text_mode);
        self.auto_switch
            .set_active(settings.auto_switch_on_playback);
        self.a2dp_reset.set_active(settings.a2dp_reset);
        self.hires_mic_agc.set_active(settings.hires_mic_agc);
        self.hires_mic_pause_convo
            .set_active(settings.hires_mic_pause_convo);
        self.stem_control.set_active(settings.stem_control);
    }
}

fn group(title: &str, rows: &[&gtk::Widget]) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(title).build();
    for row in rows {
        group.add(*row);
    }
    group
}

/// A combo row over `options` that sends the setting the user picks.
fn choice_row<T: Copy + 'static>(
    dispatch: &Dispatch,
    title: &str,
    subtitle: Option<&str>,
    options: &'static [T],
    label: impl Fn(&T) -> String,
    change: fn(T) -> SettingChange,
) -> Guarded<adw::ComboRow> {
    let labels: Vec<String> = options.iter().map(label).collect();
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    let dispatch = dispatch.clone();
    combo_row(title, subtitle, &labels, move |index| {
        if let Some(option) = options.get(index as usize) {
            dispatch.send(Input::Setting(change(*option)));
        }
    })
}

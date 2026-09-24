// use crate::bluetooth::att::ATTManager;
use {
    crate::{
        audio::mic_test,
        bluetooth::aacp::{AACPManager, ControlCommandIdentifiers},
        devices::enums::{AirPodsState, DeviceData, DeviceInformation, DeviceState},
        ui::{
            equalizer::equalizer_section,
            window::{Message, MicTest},
        },
    },
    iced::{
        Alignment::End,
        Background, Border, Center, Color, Length, Padding, Theme,
        border::Radius,
        overlay::menu,
        widget::{
            Space, button, column, combo_box, container, row,
            rule::{self, FillMode},
            scrollable, slider, text, text_input, toggler,
        },
    },
    std::{collections::HashMap, sync::Arc, time::Duration},
    tracing::error,
};

pub fn airpods_view<'a>(
    mac: &'a str,
    devices_list: &HashMap<String, DeviceData>,
    state: &'a AirPodsState,
    aacp_manager: Arc<AACPManager>,
    pause_convo: bool,
    mic_test: &'a MicTest,
    name_draft: Option<&'a str>,
    // att_manager: Arc<ATTManager>
) -> iced::widget::Container<'a, Message> {
    let mac = mac.to_string();
    // order: name, noise control, press and hold config, call controls (not sure if why it might be needed, adding it just in case), audio (personalized volume, conversational awareness, adaptive audio slider), connection settings, microphone, head gestures (not adding this), off listening mode, device information

    let name_hint = name_draft.map(|draft| match validate_device_name(draft) {
        Ok(_) => "Press Enter to rename",
        Err(e) => e,
    });
    let mut rename_col = column![
        row![
            Space::new().width(10),
            text("Name").size(16).style(|theme: &Theme| {
                let mut style = text::Style::default();
                style.color = Some(theme.palette().text);
                style
            }),
            Space::new().width(Length::Fill),
            text_input("", name_draft.unwrap_or(&state.device_name))
                .padding(Padding {
                    top: 5.0,
                    bottom: 5.0,
                    left: 10.0,
                    right: 10.0,
                })
                .style(|theme: &Theme, _status| {
                    text_input::Style {
                        background: Background::Color(Color::TRANSPARENT),
                        border: Default::default(),
                        icon: Default::default(),
                        placeholder: theme.palette().text.scale_alpha(0.7),
                        value: theme.palette().text,
                        selection: Default::default(),
                    }
                })
                .align_x(End)
                .on_input({
                    let mac = mac.clone();
                    move |name| Message::RenameInput(mac.clone(), name)
                })
                .on_submit(Message::RenameSubmit(mac.clone()))
        ]
        .align_y(Center),
    ];
    if let Some(hint) = name_hint {
        rename_col = rename_col.push(
            row![Space::new().width(Length::Fill), dim_text(hint.to_string())].padding(Padding {
                right: 10.0,
                ..Padding::ZERO
            }),
        );
    }
    let rename_input = container(rename_col)
        .padding(Padding {
            top: 5.0,
            bottom: 5.0,
            left: 10.0,
            right: 10.0,
        })
        .style(|theme: &Theme| {
            let mut style = container::Style::default();
            style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
            let mut border = Border::default();
            border.color = theme.palette().primary.scale_alpha(0.5);
            style.border = border.rounded(16);
            style
        });

    let listening_mode = container(
        row![
            text("Listening Mode").size(16).style(|theme: &Theme| {
                let mut style = text::Style::default();
                style.color = Some(theme.palette().text);
                style
            }),
            Space::new().width(Length::Fill),
            {
                let state_clone = state.clone();
                let mac = mac.clone();
                // this combo_box doesn't go really well with the design, but I am not writing my own dropdown menu for this
                combo_box(
                    &state.noise_control_state,
                    "Select Listening Mode",
                    Some(&state.noise_control_mode.clone()),
                    {
                        let aacp_manager = aacp_manager.clone();
                        move |selected_mode| {
                            let aacp_manager = aacp_manager.clone();
                            let selected_mode_c = selected_mode.clone();
                            aacp_manager.runtime().clone().spawn(async move {
                                aacp_manager
                                    .send_control_command(
                                        ControlCommandIdentifiers::ListeningMode,
                                        &[selected_mode_c.to_byte()],
                                    )
                                    .await
                                    .unwrap_or_else(|e| {
                                        error!("Failed to send Noise Control Mode command: {}", e)
                                    });
                            });
                            let mut state = state_clone.clone();
                            state.noise_control_mode = selected_mode.clone();
                            Message::StateChanged(mac.to_string(), DeviceState::AirPods(state))
                        }
                    },
                )
                .width(Length::from(200))
                .input_style(|theme: &Theme, _status| text_input::Style {
                    background: Background::Color(theme.palette().primary.scale_alpha(0.2)),
                    border: Border {
                        width: 1.0,
                        color: theme.palette().text.scale_alpha(0.3),
                        radius: Radius::from(4.0),
                    },
                    icon: Default::default(),
                    placeholder: theme.palette().text,
                    value: theme.palette().text,
                    selection: Default::default(),
                })
                .padding(Padding {
                    top: 5.0,
                    bottom: 5.0,
                    left: 10.0,
                    right: 10.0,
                })
                .menu_style(|theme: &Theme| menu::Style {
                    background: Background::Color(theme.palette().background),
                    border: Border {
                        width: 1.0,
                        color: theme.palette().text,
                        radius: Radius::from(4.0),
                    },
                    text_color: theme.palette().text,
                    selected_text_color: theme.palette().text,
                    selected_background: Background::Color(
                        theme.palette().primary.scale_alpha(0.3),
                    ),
                    shadow: Default::default(),
                })
            }
        ]
        .align_y(Center),
    )
    .padding(Padding {
        top: 5.0,
        bottom: 5.0,
        left: 18.0,
        right: 18.0,
    })
    .style(|theme: &Theme| {
        let mut style = container::Style::default();
        style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
        let mut border = Border::default();
        border.color = theme.palette().primary.scale_alpha(0.5);
        style.border = border.rounded(16);
        style
    });

    let mac_audio = mac.clone();
    let mac_information = mac.clone();

    let audio_settings_col = column![
        container(
            text("Audio Settings").size(18).style(
                |theme: &Theme| {
                    let mut style = text::Style::default();
                    style.color = Some(theme.palette().primary);
                    style
                }
            )
        )
        .padding(Padding{
            top: 5.0,
            bottom: 5.0,
            left: 18.0,
            right: 18.0,
        }),

        container(
            column![
                {
                    let aacp_manager_pv = aacp_manager.clone();
                    row![
                        column![
                            text("Personalized Volume").size(16),
                            text("Adjusts the volume in response to your environment.").size(12).style(
                                |theme: &Theme| {
                                    let mut style = text::Style::default();
                                    style.color = Some(theme.palette().text.scale_alpha(0.7));
                                    style
                                }
                            ).width(Length::Fill),
                        ].width(Length::Fill),
                        toggler(state.personalized_volume_enabled)
                            .on_toggle(
                            {
                                let mac = mac_audio.clone();
                                let state = state.clone();
                                move |is_enabled| {
                                    let aacp_manager = aacp_manager_pv.clone();
                                    let mac = mac.clone();
                                    aacp_manager.runtime().clone().spawn(
                                        async move {
                                            aacp_manager.send_control_command(
                                                ControlCommandIdentifiers::AdaptiveVolumeConfig,
                                                if is_enabled { &[0x01] } else { &[0x02] }
                                            ).await
                                    .unwrap_or_else(|e| error!("Failed to send Personalized Volume command: {}", e));
                                        }
                                    );
                                    let mut state = state.clone();
                                    state.personalized_volume_enabled = is_enabled;
                                    Message::StateChanged(mac, DeviceState::AirPods(state))
                                }
                            }
                        )
                        .spacing(0)
                        .size(20)
                    ]
                    .align_y(Center)
                    .spacing(8)
                },
                rule::horizontal(1).style(
                    |theme: &Theme| {
                        rule::Style {
                            color: theme.palette().text.scale_alpha(0.2),
                            radius: Radius::from(12),
                            fill_mode: FillMode::Full,
                            snap: false
                        }
                    }
                ),
                {
                    let aacp_manager_conv_detect = aacp_manager.clone();
                    row![
                        column![
                            text("Conversation Awareness").size(16),
                            text("Lowers the volume of your audio when it detects that you are speaking.").size(12).style(
                                |theme: &Theme| {
                                    let mut style = text::Style::default();
                                    style.color = Some(theme.palette().text.scale_alpha(0.7));
                                    style
                                }
                            ).width(Length::Fill),
                        ].width(Length::Fill),
                        {
                            // Locked while a capture manages it, so the user can't fight the override.
                            let convo_toggler = toggler(state.conversation_awareness_enabled)
                                .spacing(0)
                                .size(20);
                            if pause_convo && aacp_manager.mic_active() {
                                convo_toggler
                            } else {
                                convo_toggler.on_toggle(move |is_enabled| {
                                    let aacp_manager = aacp_manager_conv_detect.clone();
                                    aacp_manager.runtime().clone().spawn(
                                        async move {
                                            aacp_manager.set_conversation_detection(is_enabled).await;
                                        }
                                    );
                                    let mut state = state.clone();
                                    state.conversation_awareness_enabled = is_enabled;
                                    Message::StateChanged(mac_audio.to_string(), DeviceState::AirPods(state))
                                })
                            }
                        }
                    ]
                    .align_y(Center)
                    .spacing(8)
                }
            ]
                .spacing(4)
                .padding(8)
        )
        .padding(Padding{
            top: 5.0,
            bottom: 5.0,
            left: 10.0,
            right: 10.0,
        })
        .style(
            |theme: &Theme| {
                let mut style = container::Style::default();
                style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
                let mut border = Border::default();
                border.color = theme.palette().primary.scale_alpha(0.5);
                style.border = border.rounded(16);
                style
            }
        )
    ];

    let off_listening_mode_toggle = {
        let aacp_manager_olm = aacp_manager.clone();
        let mac = mac.clone();
        container(row![
            column![
                text("Off Listening Mode").size(16),
                text("When this is on, AirPods listening modes will include an Off option. Loud sound levels are not reduced when listening mode is set to Off.").size(12).style(
                    |theme: &Theme| {
                        let mut style = text::Style::default();
                        style.color = Some(theme.palette().text.scale_alpha(0.7));
                        style
                    }
                ).width(Length::Fill)
            ].width(Length::Fill),
            toggler(state.allow_off_mode)
                .on_toggle(move |is_enabled| {
                    let aacp_manager = aacp_manager_olm.clone();
                    aacp_manager.runtime().clone().spawn(
                        async move {
                            aacp_manager.send_control_command(
                                ControlCommandIdentifiers::AllowOffOption,
                                if is_enabled { &[0x01] } else { &[0x02] }
                            ).await
                                    .unwrap_or_else(|e| error!("Failed to send Off Listening Mode command: {}", e));
                        }
                    );
                    let mut state = state.clone();
                    state.allow_off_mode = is_enabled;
                    Message::StateChanged(mac.to_string(), DeviceState::AirPods(state))
                })
            .spacing(0)
            .size(20)
        ]
            .align_y(Center)
            .spacing(8)
        )
            .padding(Padding{
                top: 5.0,
                bottom: 5.0,
                left: 18.0,
                right: 18.0,
            })
            .style(
                |theme: &Theme| {
                    let mut style = container::Style::default();
                    style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
                    let mut border = Border::default();
                    border.color = theme.palette().primary.scale_alpha(0.5);
                    style.border = border.rounded(16);
                    style
                }
            )
    };

    let hires_mic_toggle = {
        let aacp_manager_mic = aacp_manager.clone();
        let mac = mac.clone();
        let state = state.clone();
        let mic_active = aacp_manager.mic_active();
        let mic_app = aacp_manager.mic_app();
        let level = aacp_manager.mic_level().clamp(0.0, 1.0);
        let hires_enabled = state.hires_mic_enabled;

        let header = row![
            column![
                text("Hi-Res Microphone").size(16),
                text("Captures the AirPods' high-quality AAC-ELD microphone stream and exposes it as an 'AirPodsHiRes' input.").size(12).style(
                    |theme: &Theme| {
                        let mut style = text::Style::default();
                        style.color = Some(theme.palette().text.scale_alpha(0.7));
                        style
                    }
                ).width(Length::Fill)
            ].width(Length::Fill),
            toggler(state.hires_mic_enabled)
                .on_toggle(move |is_enabled| {
                    let aacp_manager = aacp_manager_mic.clone();
                    aacp_manager.runtime().clone().spawn(async move {
                        aacp_manager.set_hires_mic_enabled(is_enabled).await;
                    });
                    let mut state = state.clone();
                    state.hires_mic_enabled = is_enabled;
                    Message::StateChanged(mac.to_string(), DeviceState::AirPods(state))
                })
            .spacing(0)
            .size(20)
        ]
            .align_y(Center)
            .spacing(8);

        let mut content = column![header].spacing(10);
        if mic_active {
            content = content.push(level_meter(level, mic_app));
        }
        if hires_enabled {
            content = content.push(mic_test_row(mic_test));
        }

        container(content)
            .padding(Padding {
                top: 5.0,
                bottom: 5.0,
                left: 18.0,
                right: 18.0,
            })
            .style(|theme: &Theme| {
                let mut style = container::Style::default();
                style.background =
                    Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
                let mut border = Border::default();
                border.color = theme.palette().primary.scale_alpha(0.5);
                style.border = border.rounded(16);
                style
            })
    };

    let mut information_col = column![];
    if let Some(device) = devices_list.get(mac_information.as_str()) {
        if let Some(DeviceInformation::AirPods(ref airpods_info)) = device.information {
            let info_rows = column![
                row![
                    text("Model Number").size(16).style(|theme: &Theme| {
                        let mut style = text::Style::default();
                        style.color = Some(theme.palette().text);
                        style
                    }),
                    Space::new().width(Length::Fill),
                    text(airpods_info.model_number.clone()).size(16)
                ],
                row![
                    text("Manufacturer").size(16).style(|theme: &Theme| {
                        let mut style = text::Style::default();
                        style.color = Some(theme.palette().text);
                        style
                    }),
                    Space::new().width(Length::Fill),
                    text(airpods_info.manufacturer.clone()).size(16)
                ],
                row![
                    text("Serial Number").size(16).style(|theme: &Theme| {
                        let mut style = text::Style::default();
                        style.color = Some(theme.palette().text);
                        style
                    }),
                    Space::new().width(Length::Fill),
                    button(text(airpods_info.serial_number.clone()).size(16))
                        .style(|theme: &Theme, _status| {
                            let mut style = button::Style::default();
                            style.text_color = theme.palette().text;
                            style.background = Some(Background::Color(Color::TRANSPARENT));
                            style
                        })
                        .padding(0)
                        .on_press(Message::CopyToClipboard(airpods_info.serial_number.clone()))
                ],
                row![
                    text("Left Serial Number").size(16).style(|theme: &Theme| {
                        let mut style = text::Style::default();
                        style.color = Some(theme.palette().text);
                        style
                    }),
                    Space::new().width(Length::Fill),
                    button(text(airpods_info.left_serial_number.clone()).size(16))
                        .style(|theme: &Theme, _status| {
                            let mut style = button::Style::default();
                            style.text_color = theme.palette().text;
                            style.background = Some(Background::Color(Color::TRANSPARENT));
                            style
                        })
                        .padding(0)
                        .on_press(Message::CopyToClipboard(
                            airpods_info.left_serial_number.clone()
                        ))
                ],
                row![
                    text("Right Serial Number").size(16).style(|theme: &Theme| {
                        let mut style = text::Style::default();
                        style.color = Some(theme.palette().text);
                        style
                    }),
                    Space::new().width(Length::Fill),
                    button(text(airpods_info.right_serial_number.clone()).size(16))
                        .style(|theme: &Theme, _status| {
                            let mut style = button::Style::default();
                            style.text_color = theme.palette().text;
                            style.background = Some(Background::Color(Color::TRANSPARENT));
                            style
                        })
                        .padding(0)
                        .on_press(Message::CopyToClipboard(
                            airpods_info.right_serial_number.clone()
                        ))
                ],
                row![
                    text("Version 1").size(16).style(|theme: &Theme| {
                        let mut style = text::Style::default();
                        style.color = Some(theme.palette().text);
                        style
                    }),
                    Space::new().width(Length::Fill),
                    text(airpods_info.version1.clone()).size(16)
                ],
                row![
                    text("Version 2").size(16).style(|theme: &Theme| {
                        let mut style = text::Style::default();
                        style.color = Some(theme.palette().text);
                        style
                    }),
                    Space::new().width(Length::Fill),
                    text(airpods_info.version2.clone()).size(16)
                ],
                row![
                    text("Version 3").size(16).style(|theme: &Theme| {
                        let mut style = text::Style::default();
                        style.color = Some(theme.palette().text);
                        style
                    }),
                    Space::new().width(Length::Fill),
                    text(airpods_info.version3.clone()).size(16)
                ]
            ]
            .spacing(4)
            .padding(8);

            information_col = column![
                container(text("Device Information").size(18).style(|theme: &Theme| {
                    let mut style = text::Style::default();
                    style.color = Some(theme.palette().primary);
                    style
                }))
                .padding(Padding {
                    top: 5.0,
                    bottom: 5.0,
                    left: 18.0,
                    right: 18.0,
                }),
                container(info_rows)
                    .padding(Padding {
                        top: 5.0,
                        bottom: 5.0,
                        left: 10.0,
                        right: 10.0,
                    })
                    .style(|theme: &Theme| {
                        let mut style = container::Style::default();
                        style.background =
                            Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
                        let mut border = Border::default();
                        border.color = theme.palette().primary.scale_alpha(0.5);
                        style.border = border.rounded(16);
                        style
                    })
            ];
        } else {
            error!(
                "Expected AirPodsInformation for device {}, got something else",
                mac.clone()
            );
        }
    }

    let content = container(column![
        rename_input,
        Space::new().height(Length::from(20)),
        listening_mode,
        Space::new().height(Length::from(20)),
        audio_settings_col,
        Space::new().height(Length::from(20)),
        equalizer_section(&mac, devices_list, state, &aacp_manager),
        off_listening_mode_toggle,
        Space::new().height(Length::from(20)),
        hires_mic_toggle,
        Space::new().height(Length::from(20)),
        crate::ui::airpods_settings::settings_sections(&mac, state, aacp_manager.clone()),
        information_col
    ])
    .padding(20)
    .center_x(Length::Fill);

    container(scrollable(content).height(Length::Fill)).height(Length::Fill)
}

/// Longest name the AirPods accept in a rename packet, in bytes.
const MAX_NAME_BYTES: usize = 32;

/// The name to send, trimmed, or a short hint saying why it cannot be sent.
pub(crate) fn validate_device_name(name: &str) -> Result<&str, &'static str> {
    let name = name.trim();
    if name.is_empty() {
        Err("Name can't be empty")
    } else if name.len() > MAX_NAME_BYTES {
        Err("Name is too long")
    } else {
        Ok(name)
    }
}

fn mmss(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn dim_text<'a>(content: String) -> iced::widget::Text<'a> {
    text(content).size(12).style(|theme: &Theme| {
        let mut style = text::Style::default();
        style.color = Some(theme.palette().text.scale_alpha(0.7));
        style
    })
}

fn mic_test_row<'a>(mic_test: &'a MicTest) -> iced::widget::Column<'a, Message> {
    let title = text("Microphone test").size(14);
    match mic_test {
        MicTest::Idle | MicTest::Failed(_) => {
            let hint = match mic_test {
                MicTest::Failed(e) => e.clone(),
                _ => "Record yourself, then play it back to hear what apps receive. Music is paused until you press Done.".to_string(),
            };
            column![
                title,
                row![
                    dim_text(hint).width(Length::Fill),
                    button(text("Record").size(14)).on_press(Message::MicTestRecord),
                ]
                .align_y(Center)
                .spacing(12),
            ]
            .spacing(6)
        },
        MicTest::Starting => column![title, dim_text("Pausing media…".to_string())].spacing(6),
        MicTest::Stopping => {
            column![title, dim_text("Finishing the recording…".to_string())].spacing(6)
        },
        MicTest::Recording(recorder) => column![
            title,
            row![
                text(format!("Recording  {}", mmss(recorder.elapsed())))
                    .size(14)
                    .width(Length::Fill),
                dim_text(format!("max {}", mmss(mic_test::MAX_RECORDING))),
                button(text("Stop").size(14)).on_press(Message::MicTestStop),
            ]
            .align_y(Center)
            .spacing(12),
        ]
        .spacing(6),
        MicTest::Ready(player) => {
            let duration = player.duration();
            let position = player.position().min(duration);
            let play_pause = if player.is_playing() {
                button(text("Pause").size(14)).on_press(Message::MicTestPause)
            } else {
                button(text("Play").size(14)).on_press(Message::MicTestPlay)
            };
            let skip = mic_test::SKIP.as_secs();
            let mut col = column![
                title,
                row![
                    slider(
                        0.0..=duration.as_secs_f32(),
                        position.as_secs_f32(),
                        Message::MicTestSeek
                    )
                    .step(0.1),
                    dim_text(format!("{} / {}", mmss(position), mmss(duration))),
                ]
                .align_y(Center)
                .spacing(12),
                row![
                    button(text(format!("-{skip}s")).size(14))
                        .on_press(Message::MicTestSkip(false)),
                    play_pause,
                    button(text(format!("+{skip}s")).size(14)).on_press(Message::MicTestSkip(true)),
                    Space::new().width(Length::Fill),
                    button(text("Record again").size(14)).on_press(Message::MicTestRecord),
                    button(text("Done").size(14)).on_press(Message::MicTestDone),
                ]
                .align_y(Center)
                .spacing(8),
            ]
            .spacing(6);
            if let Some(e) = player.error() {
                col = col.push(dim_text(e));
            }
            col
        },
    }
}

fn level_meter<'a>(level: f32, app: Option<String>) -> iced::widget::Container<'a, Message> {
    let filled = (level * 1000.0).round() as u16;
    let rest = 1000u16.saturating_sub(filled);
    let hot = level >= 0.9;
    let label = match app {
        Some(app) => format!("In use by {}", app),
        None => "Input level".to_string(),
    };

    let bar = container(
        row![
            container(Space::new())
                .width(Length::FillPortion(filled))
                .height(Length::Fill)
                .style(move |theme: &Theme| {
                    let mut s = container::Style::default();
                    let color = if hot {
                        theme.palette().warning
                    } else {
                        theme.palette().success
                    };
                    s.background = Some(Background::Color(color));
                    s.border = Border::default().rounded(4);
                    s
                }),
            container(Space::new()).width(Length::FillPortion(rest)),
        ]
        .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::from(10))
    .style(|theme: &Theme| {
        let mut s = container::Style::default();
        s.background = Some(Background::Color(theme.palette().text.scale_alpha(0.12)));
        s.border = Border::default().rounded(4);
        s
    });

    container(
        column![
            text(label).size(12).style(|theme: &Theme| {
                let mut style = text::Style::default();
                style.color = Some(theme.palette().text.scale_alpha(0.7));
                style
            }),
            bar,
        ]
        .spacing(4),
    )
    .width(Length::Fill)
    .padding(Padding {
        bottom: 6.0,
        ..Padding::ZERO
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmss_pads_seconds() {
        assert_eq!(mmss(Duration::ZERO), "0:00");
        assert_eq!(mmss(Duration::from_millis(59_999)), "0:59");
        assert_eq!(mmss(Duration::from_secs(65)), "1:05");
        assert_eq!(mmss(Duration::from_secs(300)), "5:00");
        assert_eq!(mmss(Duration::from_secs(3600)), "60:00");
    }

    #[test]
    fn device_name_is_trimmed_and_bounded() {
        assert_eq!(validate_device_name("  Pods  "), Ok("Pods"));
        assert!(validate_device_name("").is_err());
        assert!(validate_device_name("   ").is_err());
        let longest = "a".repeat(MAX_NAME_BYTES);
        assert_eq!(validate_device_name(&longest), Ok(longest.as_str()));
        assert!(validate_device_name(&"a".repeat(MAX_NAME_BYTES + 1)).is_err());
        // Eleven three-byte characters are 33 bytes: the limit is in bytes, not chars.
        assert!(validate_device_name(&"\u{20AC}".repeat(11)).is_err());
        assert!(validate_device_name(&"\u{20AC}".repeat(10)).is_ok());
    }
}

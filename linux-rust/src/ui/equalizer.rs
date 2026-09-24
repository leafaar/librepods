use {
    crate::{
        bluetooth::{
            aacp::AACPManager,
            eq::{BAND_MAX, CustomEq, model_supports_custom_eq},
        },
        devices::enums::{AirPodsState, DeviceData, DeviceInformation, DeviceState},
        ui::window::Message,
    },
    iced::{
        Background, Border, Center, Element, Length, Padding, Theme,
        border::Radius,
        widget::{
            Space, button, column, container, row,
            rule::{self, FillMode},
            slider, text, toggler,
        },
    },
    std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    },
    tokio::sync::Mutex,
    tracing::error,
};

/// How long the bands must stay still before they are sent. A slider drag
/// reports every step it passes; the AirPods only need where it settles.
const SEND_DELAY: Duration = Duration::from_millis(150);

/// The custom EQ shown for one device and the bookkeeping that paces sends.
#[derive(Clone, Debug)]
pub struct EqualizerState {
    pub eq: CustomEq,
    sends: Arc<Sends>,
}

/// Shared by every clone of the device state, since each UI change produces a
/// new clone.
#[derive(Debug, Default)]
struct Sends {
    /// Bumped on every change. A delayed send only goes out if no newer
    /// change happened while it waited.
    generation: AtomicU64,
    /// Held while checking the generation and sending, so an older delayed
    /// send cannot reach the AirPods after a newer immediate one.
    order: Mutex<()>,
}

impl EqualizerState {
    pub fn new(eq: CustomEq) -> Self {
        Self {
            eq,
            sends: Arc::default(),
        }
    }

    fn send_now(&self, aacp_manager: &Arc<AACPManager>) {
        self.sends.generation.fetch_add(1, Ordering::Relaxed);
        let sends = self.sends.clone();
        let aacp_manager_c = aacp_manager.clone();
        let eq = self.eq;
        aacp_manager.runtime().spawn(async move {
            let _order = sends.order.lock().await;
            send(&aacp_manager_c, &eq).await;
        });
    }

    fn send_later(&self, aacp_manager: &Arc<AACPManager>) {
        let generation = self
            .sends
            .generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let sends = self.sends.clone();
        let aacp_manager_c = aacp_manager.clone();
        let eq = self.eq;
        aacp_manager.runtime().spawn(async move {
            tokio::time::sleep(SEND_DELAY).await;
            let _order = sends.order.lock().await;
            if sends.generation.load(Ordering::Relaxed) == generation {
                send(&aacp_manager_c, &eq).await;
            }
        });
    }
}

async fn send(aacp_manager: &AACPManager, eq: &CustomEq) {
    if let Err(e) = aacp_manager.send_custom_eq(eq).await {
        error!("Failed to send custom EQ: {}", e);
    }
}

/// Equalizer card for the AirPods page, followed by a spacer. Empty for models
/// the Android app does not offer the equalizer on.
pub fn equalizer_section<'a>(
    mac: &str,
    devices_list: &HashMap<String, DeviceData>,
    state: &AirPodsState,
    aacp_manager: &Arc<AACPManager>,
) -> Element<'a, Message> {
    let model_number = devices_list.get(mac).and_then(|d| match &d.information {
        Some(DeviceInformation::AirPods(info)) => Some(info.model_number.as_str()),
        _ => None,
    });
    if !model_supports_custom_eq(model_number) {
        return column![].into();
    }

    let eq = state.custom_eq.eq;

    let toggle_row = {
        let mac = mac.to_string();
        let state = state.clone();
        let aacp_manager = aacp_manager.clone();
        row![
            column![
                text("Custom EQ").size(16),
                dim_text("Use your own bass, mids and treble instead of the recommended tuning. Needs the latest AirPods beta firmware.")
                    .width(Length::Fill),
            ]
            .width(Length::Fill),
            toggler(eq.enabled)
                .on_toggle(move |enabled| {
                    let mut state = state.clone();
                    state.custom_eq.eq.enabled = enabled;
                    state.custom_eq.send_now(&aacp_manager);
                    Message::StateChanged(mac.clone(), DeviceState::AirPods(state))
                })
                .spacing(0)
                .size(20)
        ]
        .align_y(Center)
        .spacing(8)
    };

    let mut rows = column![toggle_row].spacing(4).padding(8);
    if eq.enabled {
        let reset = {
            let mac = mac.to_string();
            let mut state = state.clone();
            let flat = CustomEq {
                enabled: true,
                ..CustomEq::default()
            };
            let mut reset = button(text("Reset").size(14));
            if eq != flat {
                state.custom_eq.eq = flat;
                let aacp_manager = aacp_manager.clone();
                // Sends from the press handler, the same way the other
                // controls on the AirPods page do.
                reset = reset.on_press_with(move || {
                    state.custom_eq.send_now(&aacp_manager);
                    Message::StateChanged(mac.clone(), DeviceState::AirPods(state.clone()))
                });
            }
            reset
        };
        rows = rows
            .push(separator())
            .push(band_row("Low", eq.low, mac, state, aacp_manager, |e, v| {
                e.low = v
            }))
            .push(band_row("Mid", eq.mid, mac, state, aacp_manager, |e, v| {
                e.mid = v
            }))
            .push(band_row(
                "High",
                eq.high,
                mac,
                state,
                aacp_manager,
                |e, v| e.high = v,
            ))
            .push(row![Space::new().width(Length::Fill), reset]);
    }

    column![
        container(
            text("Equalizer")
                .size(18)
                .style(|theme: &Theme| text::Style {
                    color: Some(theme.palette().primary),
                })
        )
        .padding(Padding {
            top: 5.0,
            bottom: 5.0,
            left: 18.0,
            right: 18.0,
        }),
        container(rows)
            .padding(Padding {
                top: 5.0,
                bottom: 5.0,
                left: 10.0,
                right: 10.0,
            })
            .style(|theme: &Theme| container::Style {
                background: Some(Background::Color(theme.palette().primary.scale_alpha(0.1))),
                border: Border {
                    color: theme.palette().primary.scale_alpha(0.5),
                    ..Border::default()
                }
                .rounded(16),
                ..container::Style::default()
            }),
        Space::new().height(Length::from(20)),
    ]
    .into()
}

fn band_row<'a>(
    label: &'a str,
    value: u8,
    mac: &str,
    state: &AirPodsState,
    aacp_manager: &Arc<AACPManager>,
    set: fn(&mut CustomEq, u8),
) -> Element<'a, Message> {
    let mac = mac.to_string();
    let state = state.clone();
    let aacp_manager = aacp_manager.clone();
    row![
        text(label).size(16).width(Length::Fixed(50.0)),
        slider(0..=BAND_MAX, value, move |v| {
            let mut state = state.clone();
            set(&mut state.custom_eq.eq, v);
            state.custom_eq.send_later(&aacp_manager);
            Message::StateChanged(mac.clone(), DeviceState::AirPods(state))
        }),
        dim_text(value.to_string()).width(Length::Fixed(30.0)),
    ]
    .align_y(Center)
    .spacing(12)
    .into()
}

fn separator<'a>() -> iced::widget::Rule<'a> {
    rule::horizontal(1).style(|theme: &Theme| rule::Style {
        color: theme.palette().text.scale_alpha(0.2),
        radius: Radius::from(12),
        fill_mode: FillMode::Full,
        snap: false,
    })
}

fn dim_text<'a>(content: impl text::IntoFragment<'a>) -> iced::widget::Text<'a> {
    text(content).size(12).style(|theme: &Theme| text::Style {
        color: Some(theme.palette().text.scale_alpha(0.7)),
    })
}

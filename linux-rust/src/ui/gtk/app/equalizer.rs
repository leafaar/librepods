//! Sends the custom EQ to the AirPods in the order the model asked for it.

use {
    super::Controller,
    crate::{
        bluetooth::{eq::CustomEq, managers::DeviceManagers},
        ui::gtk::{
            model::{EQ_SEND_DELAY, EqEffect, EqInput, Input},
            widgets::Dispatch,
        },
    },
    gtk::glib,
    std::{collections::HashMap, sync::Arc},
    tokio::{
        runtime::Handle,
        sync::{
            RwLock,
            mpsc::{UnboundedSender, unbounded_channel},
        },
    },
    tracing::warn,
};

/// Queue of EQs to send, per device address.
pub(super) type EqSender = UnboundedSender<(String, CustomEq)>;

impl Controller {
    pub(super) fn run_equalizer(&self, effect: EqEffect) {
        match effect {
            EqEffect::Send { mac, eq } => {
                if self.eq_sender.send((mac, eq)).is_err() {
                    warn!("The EQ sender stopped, the equalizer change is lost");
                }
            },
            EqEffect::SendLater { mac, generation } => {
                let dispatch = self.dispatch.clone();
                glib::MainContext::default().spawn_local(async move {
                    glib::timeout_future(EQ_SEND_DELAY).await;
                    dispatch.send(Input::Equalizer(EqInput::SendDue { mac, generation }));
                });
            },
        }
    }
}

/// A task on the backend runtime that sends queued EQs one at a time, so an
/// older EQ never reaches the AirPods after a newer one. When several are
/// queued for a device, only the newest is sent.
pub(super) fn spawn_eq_sender(
    backend: &Handle,
    managers: Arc<RwLock<HashMap<String, DeviceManagers>>>,
    dispatch: Dispatch,
) -> EqSender {
    let (tx, mut rx) = unbounded_channel::<(String, CustomEq)>();
    backend.spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut queued = vec![first];
            while let Ok((mac, eq)) = rx.try_recv() {
                match queued.iter_mut().find(|(m, _)| *m == mac) {
                    Some(slot) => slot.1 = eq,
                    None => queued.push((mac, eq)),
                }
            }
            for (mac, eq) in queued {
                let aacp = managers
                    .read()
                    .await
                    .get(&mac)
                    .and_then(DeviceManagers::get_aacp);
                let Some(aacp) = aacp else {
                    warn!("No AACP manager for {}, EQ not sent", mac);
                    continue;
                };
                if let Err(e) = aacp.send_custom_eq(&eq).await {
                    warn!("Failed to send custom EQ to {}: {}", mac, e);
                    dispatch.send(Input::Toast(format!("Could not change the equalizer: {e}")));
                }
            }
        }
    });
    tx
}

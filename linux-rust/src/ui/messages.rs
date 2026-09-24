use crate::bluetooth::aacp::AACPEvent;

#[derive(Debug, Clone)]
pub enum BluetoothUIMessage {
    OpenWindow,
    DeviceConnected(String),        // mac
    DeviceDisconnected(String),     // mac
    AACPUIEvent(String, AACPEvent), // mac, event
    ConnectAirPods,                 // tray: connect known AirPods to this PC
}
